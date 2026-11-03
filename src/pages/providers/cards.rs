//! The account cards: add-form, display and edit modes are one card swapping
//! contents — they share [`KEY_CARD_WIDTH`] (an edit card narrower than the
//! card it replaced reads as a layout jump). One display shape: one card per
//! account, whatever the filter. Pure rendering; all behavior lives in
//! `super` (the page impl).

use std::sync::Arc;

use gpui::{
    Anchor, App, AppContext as _, Context, Div, ElementId, Entity, Image, ImageFormat,
    InteractiveElement, IntoElement, ParentElement, Render, StatefulInteractiveElement, Styled,
    WeakEntity, Window, img, prelude::FluentBuilder as _, px, relative,
};
use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::input::{Input, InputState, Textarea};
use gpui_kit::component::menu::{DropdownMenu, PopupMenu, PopupMenuItem};
use gpui_kit::component::tag::Tag;
use gpui_kit::component::{ActiveTheme, Disableable, Icon, Sizable};

use gateway::http_client::KeyUpdate;
use gateway::probe::ProbeModel;
use gateway::store::{KeyGroupView, ProviderKeyView};
use gateway::store::{ProviderRow, RestRow};

use super::balance::balance_row;
use super::catalog::{
    PROTOCOL_CHOICES, PROVIDER_SPECS, ProviderIcon, ProviderSpec, Region, imported_logo, spec_for,
    variant_of_row, variants_of,
};
use super::chip::selectable_chip;
use super::groups::{group_chips, group_tags};
use super::kiro_import;
use super::order::DropSide;
use super::quota::quota_row;
use super::{
    CredentialMode, FormSource, KeyCardData, KeyEditor, KeyForm, KiroImportForm, LoginProgress,
    OpencodeLoginForm, ProvidersPage,
};
use crate::app::StatusTip;
use crate::fmt::mask_api_key;
use crate::pages::{LoadState, card};

/// The one width every account card takes — display and edit modes alike.
/// Sized for exactly 4 cards per row in the fixed 1380px window:
/// (1380 − sidebar 220 − page padding 2×24 − 3× gap 20) / 4 = 263.
const KEY_CARD_WIDTH: f32 = 268.;

/// Model rows shown outright on a key card; the rest fold into an
/// 「其他模型 N 个」 hint line.
const KEY_MODEL_ROWS: usize = 2;

impl ProvidersPage {
    /// The add-key form card: provider picker (catalog), label, credential,
    /// concurrency cap.
    pub(super) fn key_form_card(&self, form: &KeyForm, cx: &mut Context<Self>) -> Div {
        let catalog_spec = match form.source {
            FormSource::Catalog(index) => Some(&PROVIDER_SPECS[index]),
            FormSource::Custom { .. } => None,
        };
        let is_kiro = form.source.is_kiro();
        // The vendor whose credential is normally a console login: it offers
        // both, so the picker shows either way.
        let offers_login = form.source.is_account_login();
        // ... and this form picked it, so its one write is the device flow.
        let account_login = form.uses_account_login();
        // Only a pasted credential can be probed or whitelisted while adding:
        // Kiro parses its import locally and a login has no key to probe.
        let has_pasted_key = !is_kiro && !account_login;
        card(cx)
            .child(
                gpui::div()
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .text_color(cx.theme().foreground)
                    .child("添加账号"),
            )
            .child(
                gpui::div()
                    .flex()
                    .items_center()
                    .flex_wrap()
                    .gap_1()
                    .child(field_label("Provider", cx))
                    .children(PROVIDER_SPECS.iter().enumerate().map(|(index, spec)| {
                        let selected = form.source == FormSource::Catalog(index);
                        gpui::div()
                            .id(("key-form-provider", index))
                            .flex()
                            .items_center()
                            .gap_1()
                            .rounded_md()
                            .border_1()
                            .px_2()
                            .py_1()
                            .cursor_pointer()
                            .border_color(if selected {
                                cx.theme().primary
                            } else {
                                cx.theme().border
                            })
                            .when(selected, |this| this.bg(cx.theme().accent))
                            .hover(|style| style.bg(cx.theme().accent))
                            .on_click(cx.listener(move |this, _, window, cx| {
                                if this.key_form.as_ref().is_some_and(|form| !form.saving) {
                                    this.select_form_provider(
                                        FormSource::Catalog(index),
                                        window,
                                        cx,
                                    );
                                }
                            }))
                            .child(provider_icon(spec, 14., cx))
                            .child(
                                gpui::div()
                                    .text_size(px(12.))
                                    .text_color(cx.theme().foreground)
                                    .child(spec.display),
                            )
                    }))
                    .child({
                        let selected = matches!(form.source, FormSource::Custom { .. });
                        gpui::div()
                            .id("key-form-provider-custom")
                            .flex()
                            .items_center()
                            .gap_1()
                            .rounded_md()
                            .border_1()
                            .px_2()
                            .py_1()
                            .cursor_pointer()
                            .border_color(if selected {
                                cx.theme().primary
                            } else {
                                cx.theme().border
                            })
                            .when(selected, |this| this.bg(cx.theme().accent))
                            .hover(|style| style.bg(cx.theme().accent))
                            .on_click(cx.listener(|this, _, window, cx| {
                                let Some(form) = this.key_form.as_ref().filter(|f| !f.saving)
                                else {
                                    return;
                                };
                                if matches!(form.source, FormSource::Custom { .. }) {
                                    return;
                                }
                                this.select_form_provider(
                                    FormSource::Custom {
                                        protocol: PROTOCOL_CHOICES[0],
                                    },
                                    window,
                                    cx,
                                );
                            }))
                            .child(
                                gpui::div()
                                    .text_size(px(12.))
                                    .text_color(cx.theme().foreground)
                                    .child("自定义"),
                            )
                    }),
            )
            // 自定义 provider：协议单选 + base_url；label 兼作 provider 名。
            .when_some(
                match form.source {
                    FormSource::Custom { protocol } => Some(protocol),
                    FormSource::Catalog(_) => None,
                },
                |this, current| {
                    this.child(
                        gpui::div()
                            .flex()
                            .items_center()
                            .flex_wrap()
                            .gap_1()
                            .child(field_label("协议", cx))
                            .children(PROTOCOL_CHOICES.iter().enumerate().map(
                                |(index, choice)| {
                                    let choice = *choice;
                                    selectable_chip(
                                        choice.display.to_string(),
                                        choice == current,
                                        ElementId::from(("key-form-protocol", index)),
                                        cx,
                                    )
                                    .on_click(cx.listener(
                                        move |this, _, window, cx| {
                                            if this
                                                .key_form
                                                .as_ref()
                                                .is_some_and(|form| !form.saving)
                                            {
                                                this.select_form_provider(
                                                    FormSource::Custom { protocol: choice },
                                                    window,
                                                    cx,
                                                );
                                            }
                                        },
                                    ))
                                },
                            )),
                    )
                    .child(editor_field(
                        "Base URL",
                        Input::new(&form.base_url).disabled(form.saving),
                        None,
                        cx,
                    ))
                    .child(
                        gpui::div()
                            .text_size(px(12.))
                            .text_color(cx.theme().muted_foreground)
                            .child("Label 同时作为 provider 名（直连前缀 <label>/<model>），不能包含 /"),
                    )
                },
            )
            // 入口变体：仅 kimi/glm 有区域 × Coding Plan 之分；两个小按钮单选区域，
            // 一个 chip 切换 Coding Plan（风格对齐上面的 provider 选择按钮）。
            .when(
                catalog_spec.is_some_and(|spec| variants_of(spec).is_some()),
                |this| {
                    this.child(
                        gpui::div()
                            .flex()
                            .items_center()
                            .flex_wrap()
                            .gap_2()
                            .child(field_label("入口", cx))
                            .children(Region::ALL.map(|region| {
                                let selected = region == form.region;
                                gpui::div()
                                    .id(("key-form-region", region as usize))
                                    .rounded_md()
                                    .border_1()
                                    .px_3()
                                    .py_1()
                                    .cursor_pointer()
                                    .border_color(if selected {
                                        cx.theme().primary
                                    } else {
                                        cx.theme().border
                                    })
                                    .when(selected, |this| this.bg(cx.theme().accent))
                                    .hover(|style| style.bg(cx.theme().accent))
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        if let Some(form) = &mut this.key_form
                                            && !form.saving
                                        {
                                            form.region = region;
                                            form.probed_models = None;
                                            form.selected_models.clear();
                                            form.probing = false;
                                        }
                                        cx.notify();
                                    }))
                                    .child(
                                        gpui::div()
                                            .text_color(cx.theme().foreground)
                                            .child(region.label()),
                                    )
                            }))
                            .child({
                                let selected = form.coding_plan;
                                gpui::div()
                                    .id("key-form-coding-plan")
                                    .rounded_md()
                                    .border_1()
                                    .px_3()
                                    .py_1()
                                    .cursor_pointer()
                                    .border_color(if selected {
                                        cx.theme().primary
                                    } else {
                                        cx.theme().border
                                    })
                                    .when(selected, |this| this.bg(cx.theme().accent))
                                    .hover(|style| style.bg(cx.theme().accent))
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        if let Some(form) = &mut this.key_form
                                            && !form.saving
                                        {
                                            form.coding_plan = !form.coding_plan;
                                            form.probed_models = None;
                                            form.selected_models.clear();
                                            form.probing = false;
                                        }
                                        cx.notify();
                                    }))
                                    .child(
                                        gpui::div()
                                            .text_color(cx.theme().foreground)
                                            .child("Coding Plan"),
                                    )
                            }),
                    )
                },
            )
            .when(is_kiro, |this| match form.kiro_import.as_ref() {
                    Some(import) => this.child(self.kiro_import_fields(
                        import,
                        &form.max_concurrency,
                        form.saving,
                        cx,
                    )),
                    None => this,
                })
            // OpenCode Go takes two kinds of credential: a dropdown picks the
            // account login (default) or a pasted API key.
            .when(offers_login, |this| {
                this.child(credential_mode_field(form, cx.weak_entity(), cx))
            })
            // The login block owns the code, the console URL and the session's
            // progress; the API-key field below replaces it in the other mode.
            .when(account_login, |this| {
                this.when_some(form.opencode_login.as_ref(), |this, login| {
                    this.child(self.opencode_login_fields(login, cx))
                })
            })
            .when(!is_kiro, |this| {
                this.child(editor_field(
                    "Label",
                    Input::new(&form.label).disabled(form.saving),
                    Some(240.),
                    cx,
                ))
                .when(!account_login, |this| {
                    this.child(editor_field(
                        "API Key",
                        Input::new(&form.api_key).disabled(form.saving),
                        None,
                        cx,
                    ))
                })
                .child(editor_field(
                    "并发上限",
                    Input::new(&form.max_concurrency).disabled(form.saving),
                    Some(96.),
                    cx,
                ))
            })
            // 普通供应商才显示模型探测；Kiro 导入必须保持纯本地解析，登录账号无 key 可探测。
            .when(has_pasted_key, |this| {
                this.child(
                    gpui::div()
                        .flex()
                        .items_center()
                        .gap_2()
                        .child(field_label("模型", cx))
                        .child(
                            Button::new("key-form-probe")
                                .label(if form.probing {
                                    "获取中…"
                                } else {
                                    "获取模型"
                                })
                                .outline()
                                .compact()
                                .loading(form.probing)
                                .disabled(form.probing || form.saving)
                                .on_click(cx.listener(|this, _, _, cx| this.probe_form_models(cx))),
                        )
                        .child(
                            gpui::div()
                                .text_size(px(12.))
                                .text_color(cx.theme().muted_foreground)
                                .child("不选 = 全部模型"),
                        ),
                )
            })
            .when(has_pasted_key, |this| {
                this.when_some(form.probed_models.as_ref(), |this, models| {
                    let listed: Vec<&str> = models.iter().map(|model| model.id.as_str()).collect();
                    let orphans = form
                        .selected_models
                        .iter()
                        .filter(|id| !listed.contains(&id.as_str()));
                    this.child(
                        gpui::div()
                            .flex()
                            .flex_wrap()
                            .items_center()
                            .gap_1()
                            .children(models.iter().map(|model| {
                                let model_id = model.id.clone();
                                let selected = form.selected_models.contains(&model.id);
                                model_chip(
                                    model,
                                    selected,
                                    ElementId::from(gpui::SharedString::from(format!(
                                        "key-form-model-{model_id}"
                                    ))),
                                    cx,
                                )
                                .on_click(cx.listener(
                                    move |this, _, _, cx| {
                                        this.toggle_form_model(model_id.clone(), cx)
                                    },
                                ))
                            }))
                            .children(orphans.map(|model_id| {
                                // Selected but absent from the probe list — keep it visible and
                                // removable so a whitelist can never hide its own entries.
                                let model_id = model_id.clone();
                                orphan_chip(
                                    &model_id,
                                    ElementId::from(gpui::SharedString::from(format!(
                                        "key-form-orphan-{model_id}"
                                    ))),
                                    cx,
                                )
                                .on_click(cx.listener(
                                    move |this, _, _, cx| {
                                        this.toggle_form_model(model_id.clone(), cx)
                                    },
                                ))
                            })),
                    )
                })
            })
            .when_some(form.error.as_ref(), |this, error| {
                this.child(inline_error(error, cx))
            })
            .child(
                gpui::div()
                    .flex()
                    .justify_end()
                    .gap_2()
                    .child(
                        Button::new("key-form-cancel")
                            .label("取消")
                            .outline()
                            .compact()
                            .disabled(form.saving)
                            .on_click(cx.listener(|this, _, _, cx| {
                                if this.key_form.as_ref().is_some_and(|form| !form.saving) {
                                    this.key_form = None;
                                    cx.notify();
                                }
                            })),
                    )
                    .child(
                        Button::new("key-form-save")
                            .label(save_label(form))
                            .primary()
                            .compact()
                            // A waiting login keeps the form locked but is not a
                            // running request: no spinner for minutes on end.
                            .loading(form.saving && !login_waiting(form))
                            .disabled(form.saving)
                            .on_click(cx.listener(|this, _, window, cx| this.save_key(window, cx))),
                    ),
            )
    }

    /// The OpenCode Go login block: what the console asks of the user while a
    /// session waits, and what went wrong when one fails. Nothing here is
    /// editable — the gateway owns the session and the credential.
    fn opencode_login_fields(&self, login: &OpencodeLoginForm, cx: &mut Context<Self>) -> Div {
        let mut fields = gpui::div().flex().flex_col().gap_2().child(
            gpui::div()
                .text_size(px(12.))
                .text_color(cx.theme().muted_foreground)
                .child("浏览器里授权 OpenCode 账号；有 API key 也可以在上方切换为填写 key"),
        );
        match &login.progress {
            LoginProgress::Idle => {}
            LoginProgress::Waiting {
                user_code,
                verification_url,
                started,
                expires_in,
                ..
            } => {
                let code = user_code.clone();
                fields = fields.child(
                    gpui::div()
                        .flex()
                        .items_center()
                        .gap_2()
                        .child(field_label("验证码", cx))
                        .child(
                            gpui::div()
                                .font_weight(gpui::FontWeight::MEDIUM)
                                .text_size(px(16.))
                                .text_color(cx.theme().foreground)
                                .child(user_code.clone()),
                        )
                        .child(
                            Button::new("opencode-login-copy-code")
                                .icon(Icon::new(IconName::Copy).small())
                                .ghost()
                                .compact()
                                .on_click(cx.listener(move |_, _, _, cx| {
                                    cx.write_to_clipboard(gpui::ClipboardItem::new_string(
                                        code.clone(),
                                    ));
                                })),
                        ),
                );
                let url = verification_url.clone();
                let open_url = verification_url.clone();
                fields = fields.child(
                    gpui::div()
                        .flex()
                        .items_center()
                        .gap_2()
                        .child(field_label("链接", cx))
                        .child(
                            gpui::div()
                                .id("opencode-login-url")
                                .cursor_pointer()
                                .text_color(cx.theme().primary)
                                .child(verification_url.clone())
                                .on_click(cx.listener(move |_, _, _, cx| {
                                    cx.open_url(&open_url);
                                })),
                        )
                        .child(
                            Button::new("opencode-login-copy-url")
                                .icon(Icon::new(IconName::Copy).small())
                                .ghost()
                                .compact()
                                .on_click(cx.listener(move |_, _, _, cx| {
                                    cx.write_to_clipboard(gpui::ClipboardItem::new_string(
                                        url.clone(),
                                    ));
                                })),
                        ),
                );
                let elapsed = started.elapsed().as_secs();
                fields = fields.child(
                    gpui::div()
                        .flex()
                        .items_center()
                        .gap_3()
                        .child(
                            gpui::div()
                                .text_size(px(12.))
                                .text_color(cx.theme().muted_foreground)
                                .child(format!(
                                    "打开上面的链接完成授权（验证码已预填） · 已等待 {elapsed} 秒 · 有效期 {} 分钟",
                                    expires_in / 60
                                )),
                        )
                        .child(
                            Button::new("opencode-login-cancel")
                                .label("取消")
                                .outline()
                                .compact()
                                .on_click(
                                    cx.listener(|this, _, _, cx| this.cancel_opencode_login(cx)),
                                ),
                        ),
                );
            }
            LoginProgress::Failed(message) => {
                fields = fields.child(inline_error(message, cx));
            }
        }
        fields
    }

    fn kiro_import_fields(
        &self,
        import: &KiroImportForm,
        max_concurrency: &Entity<InputState>,
        saving: bool,
        cx: &mut Context<Self>,
    ) -> Div {
        let (existing_keys, groups): (&[KeyCardData], &[KeyGroupView]) = match &self.state {
            LoadState::Loaded(data) => (&data.keys, &data.groups),
            LoadState::Loading | LoadState::Failed(_) => (&[], &[]),
        };
        let mut fields = gpui::div().flex().flex_col().gap_2();
        fields = fields.child(
            gpui::div()
                .flex()
                .items_center()
                .gap_2()
                .child(field_label("Provider", cx))
                .child(
                    gpui::div()
                        .w(px(240.))
                        .text_color(cx.theme().muted_foreground)
                        .child(import.provider_name.read(cx).value().to_string()),
                ),
        );
        fields = fields.child(editor_field(
            "Label",
            Input::new(&import.key_label).disabled(saving),
            Some(240.),
            cx,
        ));
        fields = fields.child(editor_field(
            "并发上限",
            Input::new(max_concurrency).disabled(saving),
            Some(96.),
            cx,
        ));
        fields = fields
            .child(
                gpui::div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(field_label("分组", cx))
                    .child(
                        gpui::div()
                            .text_size(px(12.))
                            .text_color(cx.theme().muted_foreground)
                            .child("不选 = 不属于任何分组"),
                    ),
            )
            .child(group_chips(
                &import.selected_groups,
                groups,
                "kiro-import-group",
                ProvidersPage::toggle_import_group,
                cx,
            ));
        fields = fields.child(
            gpui::div()
                .flex()
                .items_start()
                .gap_2()
                .child(field_label("JSON", cx))
                .child(
                    gpui::div()
                        .flex_1()
                        .child(Textarea::new(&import.raw_json).h(px(180.)).disabled(saving)),
                ),
        );
        fields = fields.child(
            gpui::div()
                .flex()
                .items_center()
                .gap_2()
                .child(field_label("解析", cx))
                .child(
                    Button::new("kiro-parse")
                        .label("解析 JSON")
                        .outline()
                        .compact()
                        .disabled(saving)
                        .on_click(
                            cx.listener(|this, _, window, cx| this.parse_kiro_import(window, cx)),
                        ),
                )
                .child(
                    gpui::div()
                        .text_size(px(12.))
                        .text_color(cx.theme().muted_foreground)
                        .child("解析失败不会发起网络请求"),
                ),
        );
        if let Some(parsed) = &import.parsed {
            let count = parsed.account_count();
            if count > 1 {
                // A batch: the label field is a prefix; preview the numbering
                // against every loaded key label (render-time, nothing cached).
                let prefix = import.key_label.read(cx).value();
                let prefix = prefix.trim();
                let preview = if prefix.is_empty() {
                    format!("将导入 {count} 个账号（请填写 label 前缀）")
                } else {
                    let existing = existing_keys.iter().map(|card| card.key.label.as_str());
                    match kiro_import::label_span(prefix, existing, count) {
                        Some((first, last)) => format!("将导入 {count} 个账号：{first} … {last}"),
                        None => format!("将导入 {count} 个账号"),
                    }
                };
                fields = fields.child(
                    gpui::div()
                        .text_size(px(12.))
                        .text_color(cx.theme().muted_foreground)
                        .child(preview),
                );
            } else {
                fields = fields.children(parsed.summaries().map(|summary| {
                    gpui::div()
                        .flex()
                        .flex_col()
                        .gap_1()
                        .p_2()
                        .border_1()
                        .border_color(cx.theme().border)
                        .child(format!(
                            "region {} · API region {} · expires_at {}",
                            summary.region, summary.api_region, summary.expires_at
                        ))
                        .child(format!(
                            "endpoint {} · profile ARN {}",
                            super::catalog::kiro_endpoint(summary.api_region),
                            if summary.has_profile_arn {
                                "存在"
                            } else {
                                "缺失"
                            }
                        ))
                }));
            }
        }
        if let Some(progress) = import.progress.as_ref().filter(|_| !saving) {
            let failed = progress.failures.len();
            fields = fields.child(
                gpui::div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(format!(
                        "导入完成：成功 {}，失败 {failed}",
                        progress.total - failed
                    ))
                    .children(
                        progress
                            .failures
                            .iter()
                            .map(|(label, error)| inline_error(&format!("{label}: {error}"), cx)),
                    ),
            );
        }
        fields
    }

    /// One per-account key card: provider logo + label header, quota meters,
    /// model rows, status tags and row actions. `rests` are the key's active
    /// cooldowns (health info): one tag per row with a shared clear action.
    /// With [`CardDrag::Enabled`] the card is both a drag source (only from the
    /// grip dots in its top-left corner, so the card body keeps its own
    /// clicks) and a drop target — dropping on the left/right half of another
    /// card inserts before/after it. [`CardDrag::Disabled`] renders the very
    /// same card with no drag wiring at all.
    pub(super) fn key_card(
        &self,
        card_data: &KeyCardData,
        rests: &[RestRow],
        groups: &[KeyGroupView],
        now: chrono::DateTime<chrono::Utc>,
        drag: CardDrag,
        cx: &mut Context<Self>,
    ) -> Div {
        let provider = &card_data.provider;
        let key = &card_data.key;
        if let Some(editor) = self
            .key_editor
            .as_ref()
            .filter(|editor| editor.key_id == key.id)
        {
            return self.key_editor_card(editor, card_data, groups, cx);
        }
        let key_id = key.id;
        let provider_id = provider.id;
        let archived = key.archived_at.is_some();
        let spec = spec_for(&provider.name);
        let rests = live_rests(rests, now);
        let base = card(cx)
            .w(px(KEY_CARD_WIDTH))
            .when(!key.enabled || !provider.enabled, |this| this.opacity(0.6));
        // Drag visuals exist only while a drag is actually active (gpui-fast is
        // retained-mode: gating on `has_active_drag` also clears a stale
        // leftover). Both are derived from the page's single drag state, so
        // exactly one card is faded and exactly one insert line shows.
        let (dim_source, insert_side) = match (drag, &self.drag) {
            (CardDrag::Enabled, Some(state)) if cx.has_active_drag() => (
                state.dragged == key_id,
                match state.target {
                    Some((target, side)) if target == key_id && state.dragged != key_id => {
                        Some(side)
                    }
                    _ => None,
                },
            ),
            _ => (false, None),
        };
        let (surface, grip) = match drag {
            CardDrag::Disabled => (base, None),
            CardDrag::Enabled => {
                let weak_move = cx.weak_entity();
                let weak_drop = cx.weak_entity();
                let weak_grip = cx.weak_entity();
                let surface = base
                    .group("key-card")
                    .on_drag_move::<DraggedKey>(move |event, _window, cx| {
                        // The drag *origin* card's move listener fires wherever
                        // the pointer is ("as long as the drag started on this
                        // element"); only a card whose bounds actually contain
                        // the pointer may claim target status.
                        if !event.bounds.contains(&event.event.position) {
                            return;
                        }
                        // The card to the right of the halfway line lands after
                        // this one; the left half lands before it.
                        let side = if event.event.position.x >= event.bounds.center().x {
                            DropSide::After
                        } else {
                            DropSide::Before
                        };
                        weak_move
                            .update(cx, |this, cx| this.aim_card_drop(key_id, side, cx))
                            .ok();
                    })
                    .on_drop::<DraggedKey>(move |dropped, _window, cx| {
                        weak_drop
                            .update(cx, |this, cx| this.finish_card_drop(dropped.id, cx))
                            .ok();
                    })
                    .relative()
                    .when_some(insert_side, |this, side| {
                        // 2px accent line in the gap on the side the dragged
                        // card would land on (left = before, right = after).
                        let line = gpui::div()
                            .absolute()
                            .top_0()
                            .bottom_0()
                            .w(px(2.))
                            .bg(cx.theme().primary);
                        let line = match side {
                            DropSide::Before => line.left(px(-11.)),
                            DropSide::After => line.right(px(-11.)),
                        };
                        this.child(line)
                    })
                    .when(dim_source, |this| this.opacity(0.4));
                // The grip sits in the card's left padding, aligned with the
                // header logo (whose position never shifts) and hidden until
                // the card is hovered: the only grab surface on the card.
                let grip = gpui::div()
                    .id(("key-grip", key_id as u64))
                    .absolute()
                    .top(px(14.))
                    .left(px(3.5))
                    .w(px(12.))
                    .p_1()
                    .opacity(0.)
                    .group_hover("key-card", |this| this.opacity(0.5))
                    .cursor_grab()
                    .on_drag(
                        DraggedKey { id: key_id },
                        move |_dragged, _offset, _window, cx| {
                            // The label is read once, when the drag starts: the
                            // ghost needs text, but cloning it per card on every
                            // render would be a copy the drag rarely needs.
                            let label = weak_grip
                                .update(cx, |this, cx| {
                                    this.start_card_drag(provider_id, key_id, cx)
                                })
                                .unwrap_or_default();
                            cx.new(|_| DragGhost { label })
                        },
                    )
                    .flex()
                    .flex_col()
                    .gap(px(3.))
                    .children((0..3).map(|_| {
                        gpui::div()
                            .w(px(2.))
                            .h(px(2.))
                            .rounded_full()
                            .bg(cx.theme().muted_foreground)
                            .opacity(0.5)
                    }));
                (surface, Some(grip))
            }
        };
        surface
            // Header: provider icon, key label, probe age and the ⋯ menu
            // holding enable/edit/archive/delete.
            .child(
                gpui::div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(provider_icon_for(provider, spec, 18., cx))
                    .child(
                        gpui::div()
                            .flex_1()
                            .overflow_hidden()
                            .font_weight(gpui::FontWeight::MEDIUM)
                            .text_color(cx.theme().foreground)
                            .child(key.label.clone()),
                    )
                    // Concurrency is the account's, not the model's: the card
                    // carries it once, right after the label, however many
                    // models the account serves.
                    .child(concurrency_tag(key))
                    .when(archived, |this| this.child(archived_badge(cx)))
                    // The quota reading's probe age lives on the title row
                    // (配额 header row removed — the card is one row shorter).
                    .when_some(card_data.quota.as_ref(), |this, quota| {
                        this.child(probe_age_label(quota.probed_at, cx))
                    })
                    .child(key_menu_button(provider.id, key, cx.weak_entity())),
            )
            // Credential row: an account key has no key to preview, so the card
            // names the login that maintains it and the account it belongs to.
            .when_some(
                account_row(key, self.revealed_accounts.contains(&key_id), cx),
                |this, row| this.child(row),
            )
            // Tag row: the groups this account belongs to (derived from
            // `groups` at render time), then the variant markers (kimi/glm
            // only): 区域 (国内/国际) + Coding Plan — the variant decides the
            // anthropic capability, so the card says which entry it sits on.
            // Display-only; wraps when long; no row when there is no tag.
            .when_some(tag_row(key_id, provider, groups, cx), |this, row| {
                this.child(row)
            })
            // Quota: one meter per probed window, and/or a balance line. A key
            // without a reading (unknown vendor, probe never succeeded) renders
            // neither — the section collapses and the card shrinks.
            .when_some(card_data.quota.as_ref(), |this, quota| {
                let mut this = this;
                if !quota.windows.is_empty() {
                    this = this.child(quota_row(&quota.windows, quota.probed_at, cx));
                }
                if let Some(balance) = &quota.balance {
                    this = this.child(balance_row(balance, quota.probed_at, cx));
                }
                for detail in &quota.details {
                    this = this.child(
                        gpui::div()
                            .text_size(px(12.))
                            .text_color(cx.theme().muted_foreground)
                            .child(detail.clone()),
                    );
                }
                this
            })
            // Model rows: one line per whitelisted model (see `model_rows`),
            // pinned to the card's bottom. Cards in a grid row share the
            // tallest card's height while the quota meters above vary in row
            // count, so a flex-1 spacer here absorbs the slack — the model
            // block and footer align across sibling cards (same idiom as the
            // footer's clear-button spacer).
            .child(gpui::div().flex_1())
            .child(model_rows(key, cx))
            // Footer: cooldown tags + clear action. Without a cooldown an
            // empty spacer row keeps a grid row's card bottoms even.
            .map(|this| {
                if rests.is_empty() {
                    this.child(gpui::div().pt_1().child(gpui::div().flex_1()))
                } else {
                    this.child(rest_tags(key_id, &rests, now, cx))
                }
            })
            .when_some(grip, |this, grip| this.child(grip))
    }

    /// The card's edit mode: label and model inputs, save/cancel. The
    /// API key itself is read-only (rotate = delete + re-add): masked by default,
    /// with a reveal toggle and a copy button. Its plaintext is fetched when the
    /// editor opens (the list shapes carry none): until that lands, or if it
    /// fails, the row shows a placeholder and the copy button is disabled.
    fn key_editor_card(
        &self,
        editor: &KeyEditor,
        card_data: &KeyCardData,
        groups: &[KeyGroupView],
        cx: &mut Context<Self>,
    ) -> Div {
        // None = the reveal has not landed (loading), or it failed. The editor
        // stays usable for label/models meanwhile.
        let api_key: Option<String> = editor.api_key.clone();
        let copy_key = api_key.clone();
        let masked = api_key.as_deref().map(mask_api_key);
        let eye = if editor.show_key {
            IconName::EyeOff
        } else {
            IconName::Eye
        };
        let key = &card_data.key;
        card(cx)
            .w(px(KEY_CARD_WIDTH))
            .child(
                gpui::div()
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .text_color(cx.theme().foreground)
                    .child("编辑账号"),
            )
            // An account row has no key to reveal: its credential belongs to the
            // login that created it, and is renewed by logging in again.
            .when(key.auth_kind != "static", |this| {
                let hint = match key.auth_kind.as_str() {
                    "kiro_oidc" => "凭证由 Kiro 导入维护 · 重新导入以更新",
                    _ => "凭证由登录维护 · 重新登录以更新",
                };
                this.child(
                    gpui::div()
                        .text_size(px(12.))
                        .text_color(cx.theme().muted_foreground)
                        .child(hint),
                )
            })
            .when(key.auth_kind == "static", |this| this.child(
                gpui::div()
                    .flex()
                    .items_center()
                    .gap_1()
                    .child(
                        gpui::div()
                            .flex_1()
                            .min_w_0()
                            .text_size(px(12.))
                            .text_color(cx.theme().muted_foreground)
                            .map(|this| match (api_key.as_deref(), editor.show_key) {
                                // Not landed (or failed) yet — the reveal retries on
                                // the next open.
                                (None, _) => this.child("读取中…"),
                                // A revealed key wraps to as many lines as it needs.
                                (Some(plaintext), true) => this.child(
                                    gpui::div()
                                        .whitespace_normal()
                                        .line_height(relative(1.2))
                                        .child(plaintext.to_string()),
                                ),
                                (Some(_), false) => this.child(masked.unwrap_or_default()),
                            }),
                    )
                    .child(
                        Button::new(("key-editor-eye", card_data.key.id as u64))
                            .icon(Icon::new(eye).small())
                            .ghost()
                            .compact()
                            .on_click(cx.listener(|this, _, _, cx| {
                                if let Some(editor) = this.key_editor.as_mut() {
                                    editor.show_key = !editor.show_key;
                                }
                                cx.notify();
                            })),
                    )
                    .child(
                        Button::new(("key-editor-copy", card_data.key.id as u64))
                            .icon(Icon::new(IconName::Copy).small())
                            .ghost()
                            .compact()
                            .disabled(copy_key.is_none())
                            .on_click(cx.listener(move |_, _, _, cx| {
                                if let Some(plaintext) = &copy_key {
                                    cx.write_to_clipboard(gpui::ClipboardItem::new_string(
                                        plaintext.clone(),
                                    ));
                                }
                            })),
                    ),
            ))
            .child(editor_field(
                "Label",
                Input::new(&editor.label).disabled(editor.saving),
                Some(160.),
                cx,
            ))
            // 自定义 provider：协议单选 + base_url；label 兼作 provider 名。
            .when_some(editor.custom.as_ref(), |this, custom| {
                let locked = editor.saving || editor.probing;
                this.child(
                    gpui::div()
                        .flex()
                        .items_center()
                        .flex_wrap()
                        .gap_1()
                        .child(field_label("协议", cx))
                        .children(PROTOCOL_CHOICES.iter().enumerate().map(|(index, choice)| {
                            let choice = *choice;
                            selectable_chip(
                                choice.display.to_string(),
                                choice == custom.protocol,
                                ElementId::from(("key-editor-protocol", index)),
                                cx,
                            )
                            .when(!locked, |chip| {
                                chip.on_click(cx.listener(move |this, _, window, cx| {
                                    this.select_editor_protocol(choice, window, cx)
                                }))
                            })
                        })),
                )
                .child(editor_field(
                    "Base URL",
                    Input::new(&custom.base_url).disabled(locked),
                    None,
                    cx,
                ))
                .child(
                    gpui::div()
                        .text_size(px(12.))
                        .text_color(cx.theme().muted_foreground)
                        .child("Label 同时作为 provider 名；改名会改变直连前缀 <label>/<model>（路由不受影响）"),
                )
            })
            .child(editor_field(
                "并发上限",
                Input::new(&editor.max_concurrency).disabled(editor.saving),
                Some(96.),
                cx,
            ))
            // 分组归属：点击切换，可多选；保存时只在有变化时提交。
            .child(
                gpui::div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(field_label("分组", cx))
                    .child(
                        gpui::div()
                            .text_size(px(12.))
                            .text_color(cx.theme().muted_foreground)
                            .child("不选 = 不属于任何分组"),
                    ),
            )
            .child(group_chips(
                &editor.selected_groups,
                groups,
                "key-editor-group",
                ProvidersPage::toggle_editor_group,
                cx,
            ))
            // 模型白名单：chips 来自探测快照，「重新获取」刷新；不选 = 全部模型。
            .child(
                gpui::div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(field_label("模型", cx))
                    .child(
                        Button::new("key-editor-reprobe")
                            .label(if editor.probing {
                                "获取中…"
                            } else {
                                "重新获取"
                            })
                            .outline()
                            .compact()
                            .loading(editor.probing)
                            .disabled(editor.probing || editor.saving)
                            .on_click(cx.listener(|this, _, _, cx| this.reprobe_editor_models(cx))),
                    )
                    .child(
                        gpui::div()
                            .text_size(px(12.))
                            .text_color(cx.theme().muted_foreground)
                            .child("不选 = 全部模型"),
                    ),
            )
            .child(match &editor.probed_models {
                Some(models) => {
                    let listed: Vec<&str> = models.iter().map(|model| model.id.as_str()).collect();
                    let orphans = editor
                        .selected_models
                        .iter()
                        .filter(|id| !listed.contains(&id.as_str()));
                    gpui::div()
                        .flex()
                        .flex_wrap()
                        .items_center()
                        .gap_1()
                        .children(models.iter().map(|model| {
                            let model_id = model.id.clone();
                            let selected = editor.selected_models.contains(&model.id);
                            model_chip(
                                model,
                                selected,
                                ElementId::from(gpui::SharedString::from(format!(
                                    "key-editor-model-{model_id}"
                                ))),
                                cx,
                            )
                            .on_click(cx.listener(
                                move |this, _, _, cx| {
                                    this.toggle_editor_model(model_id.clone(), cx)
                                },
                            ))
                        }))
                        .children(orphans.map(|model_id| {
                            let model_id = model_id.clone();
                            orphan_chip(
                                &model_id,
                                ElementId::from(gpui::SharedString::from(format!(
                                    "key-editor-orphan-{model_id}"
                                ))),
                                cx,
                            )
                            .on_click(cx.listener(
                                move |this, _, _, cx| {
                                    this.toggle_editor_model(model_id.clone(), cx)
                                },
                            ))
                        }))
                }
                None => {
                    let orphans = editor.selected_models.iter();
                    if editor.selected_models.is_empty() {
                        gpui::div()
                            .text_size(px(12.))
                            .text_color(cx.theme().muted_foreground)
                            .child("未探测到模型列表")
                    } else {
                        // No probe list yet, but the saved whitelist is real — show it.
                        gpui::div()
                            .flex()
                            .flex_wrap()
                            .items_center()
                            .gap_1()
                            .children(orphans.map(|model_id| {
                                let model_id = model_id.clone();
                                orphan_chip(
                                    &model_id,
                                    ElementId::from(gpui::SharedString::from(format!(
                                        "key-editor-orphan-{model_id}"
                                    ))),
                                    cx,
                                )
                                .on_click(cx.listener(
                                    move |this, _, _, cx| {
                                        this.toggle_editor_model(model_id.clone(), cx)
                                    },
                                ))
                            }))
                    }
                }
            })
            .when_some(editor.error.as_ref(), |this, error| {
                this.child(inline_error(error, cx))
            })
            .child(
                gpui::div()
                    .flex()
                    .justify_end()
                    .gap_2()
                    .child(
                        Button::new("key-editor-cancel")
                            .label("取消")
                            .outline()
                            .compact()
                            .disabled(editor.saving)
                            .on_click(cx.listener(|this, _, _, cx| {
                                if this
                                    .key_editor
                                    .as_ref()
                                    .is_some_and(|editor| !editor.saving)
                                {
                                    this.key_editor = None;
                                    cx.notify();
                                }
                            })),
                    )
                    .child(
                        Button::new("key-editor-save")
                            .label(if editor.saving {
                                "保存中…"
                            } else {
                                "保存"
                            })
                            .primary()
                            .compact()
                            .loading(editor.saving)
                            .disabled(editor.saving || editor.probing)
                            .on_click(cx.listener(|this, _, _, cx| this.save_key_editor(cx))),
                    ),
            )
    }
}

/// Whether a card may be dragged to reorder the accounts. A two-variant enum
/// rather than a `bool`, because the call site reads `key_card(.., drag, cx)`
/// and the decision itself is [`super::groups::GroupFilter::allows_reorder`].
/// `Disabled` leaves the card a plain element: no bindings, no grip, no
/// insert line.
#[derive(Clone, Copy)]
pub(super) enum CardDrag {
    Enabled,
    Disabled,
}

/// Drag payload carried by an active card drag (the dragged account's id).
struct DraggedKey {
    id: i64,
}

/// The little chip following the cursor while a card is dragged.
struct DragGhost {
    label: String,
}

impl Render for DragGhost {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        gpui::div()
            .px_3()
            .py_1()
            .rounded_md()
            .border_1()
            .border_color(cx.theme().primary)
            .bg(cx.theme().accent)
            .text_size(px(12.))
            .text_color(cx.theme().foreground)
            .opacity(0.9)
            .child(self.label.clone())
    }
}

/// The cooldowns in `rests` still in force at `now`. The gateway drops expired
/// rows only when it builds the snapshot, and the page keeps rendering that
/// snapshot, so a cooldown that ended since must not show as resting.
#[must_use]
fn live_rests(rests: &[RestRow], now: chrono::DateTime<chrono::Utc>) -> Vec<&RestRow> {
    rests.iter().filter(|rest| rest.rest_until > now).collect()
}

/// The cooldown footer: one warning tag per rest row (a whole-key row omits
/// the model; the reason rides the tag's tooltip rather than the row, where a
/// long upstream message would be truncated into noise) plus the 清除 action
/// that wakes the whole key. `rests` are the [`live_rests`] at `now`; callers
/// render the footer only when there is one.
fn rest_tags(
    key_id: i64,
    rests: &[&RestRow],
    now: chrono::DateTime<chrono::Utc>,
    cx: &mut Context<ProvidersPage>,
) -> Div {
    let mut footer = gpui::div().flex().items_center().gap_1().pt_1();
    let today = now.with_timezone(&crate::ui_tz::display_tz()).date_naive();
    for (index, rest) in rests.iter().enumerate() {
        // Same calendar day shows just the clock; anything further (a monthly
        // quota reset, an 8-day cap) must carry the date.
        let until = rest.rest_until.with_timezone(&crate::ui_tz::display_tz());
        let until = if until.date_naive() == today {
            until.format("%H:%M").to_string()
        } else {
            until.format("%m-%d %H:%M").to_string()
        };
        let tag = match &rest.model {
            Some(model) => format!("冷却 {model} · {until} 恢复"),
            None => format!("冷却 · {until} 恢复"),
        };
        let id: ElementId = ("rest-tag", index as u64).into();
        let element = match rest.reason.as_ref().filter(|reason| !reason.is_empty()) {
            Some(reason) => {
                let reason = reason.clone();
                gpui::div()
                    .id(id)
                    .child(Tag::warning().child(tag))
                    .tooltip(move |_, cx| {
                        cx.new(|_| StatusTip {
                            text: reason.clone(),
                        })
                        .into()
                    })
            }
            None => gpui::div().id(id).child(Tag::warning().child(tag)),
        };
        footer = footer.child(element);
    }
    // Spacer pushes the clear button to the right edge of the footer row.
    footer.child(gpui::div().flex_1()).child(
        Button::new(("rest-clear", key_id as u64))
            .label("清除")
            .ghost()
            .compact()
            .on_click(cx.listener(move |this, _, _, cx| {
                this.write(
                    cx,
                    move |admin| async move { admin.clear_rest(key_id).await },
                );
            })),
    )
}

/// The ⋯ menu button of one account card. Only the ids and the two flags the
/// items display are captured (the menu entity is cached between opens and
/// cannot borrow the card); every click resolves the key through the page.
fn key_menu_button(
    provider_id: i64,
    key: &ProviderKeyView,
    weak: WeakEntity<ProvidersPage>,
) -> impl IntoElement {
    let shown = MenuKey {
        provider_id,
        key_id: key.id,
        enabled: key.enabled,
        archived: key.archived_at.is_some(),
    };
    Button::new(("key-menu", key.id as u64))
        .icon(Icon::new(IconName::Ellipsis).small())
        .ghost()
        .compact()
        .dropdown_menu_with_anchor(Anchor::TopRight, move |menu, _, _| {
            key_menu(menu, shown, weak.clone())
        })
}

/// What the ⋯ menu captures of its account: the ids it acts on and the state
/// its items show. `Copy`, so a render never clones a key view for a menu.
#[derive(Clone, Copy)]
struct MenuKey {
    provider_id: i64,
    key_id: i64,
    enabled: bool,
    archived: bool,
}

/// What a toggle item (启用, 归档) writes, from the key's state read back at
/// click time: the opposite of it, or `None` when there is nothing to write —
/// the key left the list after the menu was built (`Ok(None)`) or the page is
/// gone (`Err`). Never falls back to the render-time value, which would write
/// a stale target for a dead key id.
#[must_use]
fn toggle_target<E>(read: Result<Option<bool>, E>) -> Option<bool> {
    read.ok().flatten().map(|current| !current)
}

/// The account menu: 启用/停用、编辑、归档/移出归档、删除. The state the items show
/// is captured at render time; every click resolves the key again through the
/// page, because the cached menu entity can outlive the snapshot it was built
/// from, and a key that left the list is a no-op in every item.
fn key_menu(menu: PopupMenu, shown: MenuKey, weak: WeakEntity<ProvidersPage>) -> PopupMenu {
    let MenuKey {
        provider_id,
        key_id,
        enabled,
        archived,
    } = shown;
    // Breadcrumb kept from the menu's first debugging round.
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open("/tmp/cardinal/card-menu.log")
    {
        use std::io::Write as _;
        let _ = writeln!(
            file,
            "[build] menu provider={provider_id} key={key_id} captured_enabled={enabled}"
        );
    }
    menu.item(PopupMenuItem::new("启用").checked(enabled).on_click({
        let weak = weak.clone();
        // listener_for-style plumbing (the library's own pattern): the handler
        // carries the window itself, and Entity::update never consults the
        // entity→window map — which has no entry for this page and made
        // update_in fail with "entity has no current window".
        move |_, _window, cx| {
            // The menu entity is cached between opens, so the captured
            // `enabled` can go stale — read the key's current state at click
            // time, and write nothing for a key that left the list.
            let read = weak.read_with(cx, |page, _| page.key_enabled(provider_id, key_id));
            let Some(target) = toggle_target(read) else {
                return;
            };
            if let Err(error) = weak.update(cx, |this, cx| {
                this.write(cx, move |admin| async move {
                    admin
                        .update_key(
                            provider_id,
                            key_id,
                            &KeyUpdate {
                                enabled: Some(target),
                                ..Default::default()
                            },
                        )
                        .await
                });
            }) {
                eprintln!("card menu enable failed: {error}");
            }
        }
    }))
    .separator()
    .item(
        PopupMenuItem::new("编辑")
            .icon(Icon::new(IconName::Pencil).small())
            .on_click({
                let weak = weak.clone();
                move |_, window, cx| {
                    if let Err(error) = weak.update(cx, |this, cx| {
                        this.open_key_editor(provider_id, key_id, window, cx)
                    }) {
                        eprintln!("card menu edit failed: {error}");
                    }
                }
            }),
    )
    .item(
        PopupMenuItem::new(if archived { "移出归档" } else { "归档" })
            .icon(
                Icon::new(if archived {
                    IconName::ArchiveRestore
                } else {
                    IconName::Archive
                })
                .small(),
            )
            .on_click({
                let weak = weak.clone();
                move |_, _window, cx| {
                    // Cached menu entity: read the current state at click time
                    // and write nothing for a key that left the list (as 启用).
                    let read = weak.read_with(cx, |page, _| page.key_archived(provider_id, key_id));
                    let Some(target) = toggle_target(read) else {
                        return;
                    };
                    if let Err(error) = weak.update(cx, |this, cx| {
                        this.set_key_archived(provider_id, key_id, target, cx)
                    }) {
                        eprintln!("card menu archive failed: {error}");
                    }
                }
            }),
    )
    .item(
        PopupMenuItem::new("删除")
            .icon(Icon::new(IconName::Delete).small())
            .on_click({
                let weak = weak.clone();
                move |_, window, cx| {
                    if let Err(error) = weak.update(cx, |this, cx| {
                        // A key that left the list has nothing to delete.
                        if let Some(label) = this.key_label(provider_id, key_id) {
                            this.confirm_delete_key(provider_id, key_id, label, window, cx)
                        }
                    }) {
                        eprintln!("card menu delete failed: {error}");
                    }
                }
            }),
    )
}

/// The 已归档 marker on an account header.
fn archived_badge(cx: &App) -> Div {
    gpui::div()
        .flex_none()
        .rounded_md()
        .border_1()
        .px_1p5()
        .text_size(px(10.))
        .border_color(cx.theme().accent)
        .text_color(cx.theme().muted_foreground)
        .child("已归档")
}

/// The quota reading's age, shown on the header row next to the label.
fn probe_age_label(probed_at: chrono::DateTime<chrono::Utc>, cx: &App) -> Div {
    gpui::div()
        .text_size(px(11.))
        .text_color(cx.theme().muted_foreground)
        .child(crate::fmt::probe_age(probed_at, chrono::Utc::now()))
}

/// The account's concurrency occupancy, shown once per card on the header row:
/// concurrency is a limit on the account, so every model it serves shares the
/// one figure (a per-model row would print the same tag N times).
fn concurrency_tag(key: &ProviderKeyView) -> Tag {
    let text = match key.max_concurrency {
        Some(max) => format!("并发 {}/{}", key.inflight.unwrap_or(0), max),
        None => "并发 不限".to_string(),
    };
    Tag::secondary().child(text)
}

/// The key card's model rows: one line per whitelisted model (`KEY_MODEL_ROWS`
/// shown, the rest folded into an 「其他模型 N 个」 hint). A key without a
/// whitelist (= 全部模型) renders nothing — the section collapses and the card
/// shrinks. The account's concurrency sits on the header instead, because it is
/// one limit for the whole account rather than a per-model setting.
fn model_rows(key: &ProviderKeyView, cx: &App) -> Div {
    let models = key
        .allowed_models
        .as_ref()
        .filter(|models| !models.is_empty());
    let Some(models) = models else {
        return gpui::div();
    };
    let mut rows: Vec<gpui::AnyElement> = Vec::new();
    for model in models.iter().take(KEY_MODEL_ROWS) {
        rows.push(
            gpui::div()
                .flex()
                .items_center()
                .gap_1()
                .child(
                    gpui::div()
                        .flex_1()
                        .min_w(px(0.))
                        .overflow_hidden()
                        .text_ellipsis()
                        .whitespace_nowrap()
                        .text_size(px(12.))
                        .text_color(cx.theme().foreground)
                        .child(model.clone()),
                )
                .into_any_element(),
        );
    }
    if models.len() > KEY_MODEL_ROWS {
        rows.push(
            gpui::div()
                .text_size(px(11.))
                .text_color(cx.theme().muted_foreground)
                .child(format!("其他模型 {} 个", models.len() - KEY_MODEL_ROWS))
                .into_any_element(),
        );
    }
    gpui::div().flex().flex_col().gap_1().children(rows)
}

/// Provider icon at a given render size: the embedded SVG logo, rasterized
/// full-color (gpui's `svg()` element renders monochrome alpha-masks, so
/// brand logos go through `img()` instead).
fn provider_icon(spec: &'static ProviderSpec, height: f32, cx: &App) -> Div {
    let ProviderIcon::Svg(bytes) = &spec.icon;
    logo(bytes, height, cx)
}

/// A brand SVG rendered at `height` (see [`provider_icon`] for why `img()`).
fn logo(bytes: &'static [u8], height: f32, _cx: &App) -> Div {
    let image = Arc::new(Image::from_bytes(ImageFormat::Svg, bytes.to_vec()));
    gpui::div()
        .flex_shrink_0()
        .child(img(image).w(px(height)).h(px(height)))
}

/// Icon for an arbitrary provider row: catalog logo when known, the imported
/// vendor's logo when it is not in the catalog (Kiro), else a letter tile from
/// the provider's stored name.
fn provider_icon_for(
    provider: &ProviderRow,
    spec: Option<&'static ProviderSpec>,
    height: f32,
    cx: &App,
) -> Div {
    if let Some(spec) = spec {
        return provider_icon(spec, height, cx);
    }
    match imported_logo(&provider.name) {
        Some(bytes) => logo(bytes, height, cx),
        None => letter_tile(&provider.name[..1].to_uppercase(), height, cx),
    }
}

/// A small rounded tile showing one letter (fallback provider icon).
fn letter_tile(letter: &str, height: f32, cx: &App) -> Div {
    gpui::div()
        .flex_shrink_0()
        .flex()
        .items_center()
        .justify_center()
        .w(px(height))
        .h(px(height))
        .rounded_md()
        .bg(cx.theme().muted)
        .text_size(px(height * 0.55))
        .font_weight(gpui::FontWeight::MEDIUM)
        .text_color(cx.theme().muted_foreground)
        .child(letter.to_string())
}

/// The credential picker of a vendor that offers both an account login and a
/// pasted API key (OpenCode Go): a dropdown naming the current choice, so the
/// login block and the API-key field never render together.
fn credential_mode_field(form: &KeyForm, weak: WeakEntity<ProvidersPage>, cx: &App) -> Div {
    let current = form.credential_mode;
    // The set the menu offers, in display order (the default first).
    let modes = [CredentialMode::AccountLogin, CredentialMode::ApiKey];
    gpui::div()
        .flex()
        .items_center()
        .gap_2()
        .child(field_label("凭证方式", cx))
        .child(
            Button::new("key-form-credential-mode")
                .label(current.label())
                .outline()
                .compact()
                .disabled(form.saving)
                .dropdown_menu_with_anchor(Anchor::BottomLeft, move |menu, _, _| {
                    modes.into_iter().fold(menu, |menu, mode| {
                        let weak = weak.clone();
                        menu.item(
                            PopupMenuItem::new(mode.label())
                                .checked(mode == current)
                                .on_click(move |_, _, cx| {
                                    weak.update(cx, |this, cx| {
                                        this.set_form_credential_mode(mode, cx)
                                    })
                                    .ok();
                                }),
                        )
                    })
                }),
        )
}

/// The add form's save button text: the Kiro import counts its accounts
/// while running.
fn save_label(form: &KeyForm) -> String {
    if form.uses_account_login() {
        return if login_waiting(form) {
            "登录中…".to_string()
        } else {
            "开始登录".to_string()
        };
    }
    let progress = form
        .kiro_import
        .as_ref()
        .and_then(|import| import.progress.as_ref());
    match progress {
        Some(progress) if form.saving => format!("导入中 {}/{}", progress.done, progress.total),
        _ if form.saving => "验证中…".to_string(),
        _ => "保存".to_string(),
    }
}

/// Whether the add form waits on an account login: the form stays locked while
/// it does, but no request is in flight.
fn login_waiting(form: &KeyForm) -> bool {
    form.opencode_login
        .as_ref()
        .is_some_and(|login| login.progress.is_waiting())
}

/// Small fixed-width field label used by the form cards.
fn field_label(label: &'static str, cx: &App) -> Div {
    gpui::div()
        .w(px(FIELD_LABEL_W))
        .flex_shrink_0()
        .text_color(cx.theme().muted_foreground)
        .child(label)
}

/// The model chips sit on the line below the 「模型」 label/button row: no
/// label-column indent, so they align with the card's left edge (= the label's
/// edge), which reads cleaner than chips floating mid-card.
const FIELD_LABEL_W: f32 = 72.;

/// One selectable model chip for the whitelist pickers (add form and key
/// editor): model id plus the numeric limits the probe list carried
/// (id · 上下文 · 输出上限 · 推理), highlighted when selected. Input modalities
/// are deliberately not rendered — a model's accepted media is not something a
/// whitelist picker has to spell out on every chip.
fn model_chip(model: &ProbeModel, selected: bool, id: ElementId, cx: &App) -> gpui::Stateful<Div> {
    let mut chip_text = model.id.clone();
    if let Some(context) = model.context {
        chip_text.push_str(&format!(
            " · {}",
            crate::fmt::humanize_count(context as i64)
        ));
    }
    if let Some(output) = model.max_output {
        chip_text.push_str(&format!(
            " · 出 {}",
            crate::fmt::humanize_count(output as i64)
        ));
    }
    if !model.effort_levels.is_empty() {
        chip_text.push_str(" · 推理");
    }
    selectable_chip(chip_text, selected, id, cx)
}

/// A selected model that is NOT in the probed list (vendor listing incomplete,
/// or the model predates the snapshot): rendered as a muted "id · 不在列表"
/// chip so the whitelist stays visible and removable — never silently dropped.
fn orphan_chip(model_id: &str, id: ElementId, cx: &App) -> gpui::Stateful<Div> {
    gpui::div()
        .id(id)
        .cursor_pointer()
        .border_1()
        .border_color(cx.theme().danger)
        .rounded_md()
        .px_2()
        .py_0p5()
        .text_size(px(12.))
        .text_color(cx.theme().danger)
        .child(format!("{model_id} · 不在列表"))
}

/// One labeled input row of a form card. `width` fixes the input width (size
/// it to the expected content, e.g. a 4-digit cap); `None` fills the row
/// (credentials and other long values).
pub(super) fn editor_field(label: &'static str, input: Input, width: Option<f32>, cx: &App) -> Div {
    let row = gpui::div()
        .flex()
        .items_center()
        .gap_2()
        .child(field_label(label, cx));
    match width {
        Some(width) => row.child(input.w(px(width))),
        None => row.child(gpui::div().flex_1().child(input.w_full())),
    }
}

/// Inline validation/write error line inside a form card.
pub(super) fn inline_error(message: &str, cx: &App) -> Div {
    gpui::div()
        .text_size(px(12.))
        .text_color(cx.theme().danger)
        .child(message.to_string())
}

/// A card's tag row: group tags followed by the variant markers. Non-catalog
/// rows (a manually-added provider) carry no markers. `None` when empty.
fn tag_row(key_id: i64, provider: &ProviderRow, groups: &[KeyGroupView], cx: &App) -> Option<Div> {
    let mut tags = group_tags(key_id, groups, cx);
    tags.extend(variant_markers(provider, cx));
    if tags.is_empty() {
        return None;
    }
    Some(
        gpui::div()
            .flex()
            .flex_wrap()
            .items_center()
            .gap_1()
            .children(tags),
    )
}

/// The credential line of an account card, where a pasted key's preview would
/// otherwise sit. `None` for static-key rows — they add nothing beyond the key
/// itself.
fn account_row(
    key: &ProviderKeyView,
    revealed: bool,
    cx: &mut Context<ProvidersPage>,
) -> Option<Div> {
    match key.auth_kind.as_str() {
        "static" => None,
        // The OpenCode login is identified by its email alone: the login badge
        // and the workspace name say nothing about which account is in use, and
        // the address is masked until the eye is clicked.
        "opencode_oauth" => Some(opencode_account_row(key, revealed, cx)),
        // A Kiro import (and any kind this build does not know yet — shown
        // rather than hidden, so the card still says the credential is
        // login-maintained) keeps the badge, then account and organization.
        kind => {
            let kind = match kind {
                "kiro_oidc" => "Kiro 账号",
                other => other,
            };
            let mut row = gpui::div()
                .flex()
                .flex_wrap()
                .items_center()
                .gap_1()
                .child(Tag::secondary().child(kind.to_string()));
            if let Some(account) = key.account.as_deref() {
                row = row.child(
                    gpui::div()
                        .min_w(px(0.))
                        .overflow_hidden()
                        .whitespace_nowrap()
                        .text_ellipsis()
                        .text_size(px(11.))
                        .text_color(cx.theme().muted_foreground)
                        .child(account.to_string()),
                );
            }
            if let Some(organization) = key.organization.as_deref() {
                row = row.child(Tag::secondary().child(organization.to_string()));
            }
            Some(row)
        }
    }
}

/// The OpenCode account's email — masked by default — and the eye that toggles
/// between the mask and the address in clear. A login that recorded no address
/// falls back to the login badge: there is nothing to reveal.
fn opencode_account_row(
    key: &ProviderKeyView,
    revealed: bool,
    cx: &mut Context<ProvidersPage>,
) -> Div {
    let key_id = key.id;
    let Some(account) = key.account.as_deref() else {
        return gpui::div()
            .flex()
            .items_center()
            .child(Tag::secondary().child("OpenCode 账号"));
    };
    let text = if revealed {
        account.to_string()
    } else {
        crate::fmt::mask_email(account)
    };
    let eye = if revealed {
        IconName::EyeOff
    } else {
        IconName::Eye
    };
    gpui::div()
        .flex()
        .items_center()
        .gap_1()
        .min_w(px(0.))
        .child(
            gpui::div()
                .flex_1()
                .min_w(px(0.))
                .overflow_hidden()
                .whitespace_nowrap()
                .text_ellipsis()
                .text_size(px(11.))
                .text_color(cx.theme().muted_foreground)
                .child(text),
        )
        .child(
            Button::new(("account-email-eye", key_id as u64))
                .icon(Icon::new(eye).small())
                .ghost()
                .compact()
                .on_click(
                    cx.listener(move |this, _, _, cx| this.toggle_account_reveal(key_id, cx)),
                ),
        )
}

/// The variant markers of a catalog variant row (kimi/glm only): 区域
/// (国内/国际), then Coding Plan when it is one. Empty for a non-catalog row
/// (a manually-added provider). Rendered on the account card's tag row.
fn variant_markers(provider: &ProviderRow, cx: &App) -> Vec<Div> {
    let Some((region, coding_plan)) = variant_of_row(&provider.name, &provider.base_url) else {
        return Vec::new();
    };
    let mut markers: Vec<Div> = vec![variant_marker(region.label(), cx)];
    if coding_plan {
        markers.push(variant_marker("Coding Plan", cx));
    }
    markers
}

fn variant_marker(text: &'static str, cx: &App) -> Div {
    gpui::div()
        .flex_none()
        .rounded_md()
        .border_1()
        .px_1p5()
        .text_size(px(10.))
        .border_color(cx.theme().accent)
        .text_color(cx.theme().muted_foreground)
        .child(text)
}
