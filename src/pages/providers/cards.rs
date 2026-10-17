//! The account cards: add-form, display and edit modes are one card swapping
//! contents — they share [`KEY_CARD_WIDTH`] (an edit card narrower than the
//! card it replaced reads as a layout jump). Pure rendering; all behavior
//! lives in `super` (the page impl).

use std::sync::Arc;

use gpui::{
    Anchor, App, AppContext as _, Context, Div, ElementId, Image, ImageFormat, InteractiveElement,
    IntoElement, ParentElement, Render, StatefulInteractiveElement, Styled, Window, img,
    prelude::FluentBuilder as _, px, relative,
};
use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::input::Input;
use gpui_kit::component::menu::{DropdownMenu, PopupMenuItem};
use gpui_kit::component::tag::Tag;
use gpui_kit::component::{ActiveTheme, Disableable, Icon, Sizable};

use gateway::store::ProviderKeyView;
use gateway::probe::ProbeModel;
use gateway::store::{ProviderRow, RestRow};

use super::catalog::{
    PROVIDER_SPECS, ProviderIcon, ProviderSpec, Region, spec_for, variant_of_row, variants_of,
};
use super::quota::quota_row;
use super::{KeyCardData, KeyDrag, KeyEditor, KeyForm, ProvidersPage, reordered};
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
                        let selected = index == form.spec_index;
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
                            .on_click(cx.listener(move |this, _, _, cx| {
                                if let Some(form) = &mut this.key_form {
                                    form.spec_index = index;
                                    // The probed chips belong to the previously selected provider.
                                    form.probed_models = None;
                                    form.selected_models.clear();
                                }
                                cx.notify();
                            }))
                            .child(provider_icon(spec, 14., cx))
                            .child(
                                gpui::div()
                                    .text_size(px(12.))
                                    .text_color(cx.theme().foreground)
                                    .child(spec.display),
                            )
                    })),
            )
            // 入口变体：仅 kimi/glm 有区域 × Coding Plan 之分；两个小按钮单选区域，
            // 一个 chip 切换 Coding Plan（风格对齐上面的 provider 选择按钮）。
            .when(
                variants_of(&PROVIDER_SPECS[form.spec_index]).is_some(),
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
                                        if let Some(form) = &mut this.key_form {
                                            form.region = region;
                                            form.probed_models = None;
                                            form.selected_models.clear();
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
                                        if let Some(form) = &mut this.key_form {
                                            form.coding_plan = !form.coding_plan;
                                            form.probed_models = None;
                                            form.selected_models.clear();
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
            .child(editor_field(
                "Label",
                Input::new(&form.label),
                Some(240.),
                cx,
            ))
            .child(editor_field("API Key", Input::new(&form.api_key), None, cx))
            .child(editor_field(
                "并发上限",
                Input::new(&form.max_concurrency),
                Some(96.),
                cx,
            ))
            // 模型白名单：点「获取模型」探测后点选；不选 = 全部模型可用。
            .child(
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
            .when_some(form.probed_models.as_ref(), |this, models| {
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
                                move |this, _, _, cx| this.toggle_form_model(model_id.clone(), cx),
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
                                move |this, _, _, cx| this.toggle_form_model(model_id.clone(), cx),
                            ))
                        })),
                )
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
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.key_form = None;
                                cx.notify();
                            })),
                    )
                    .child(
                        Button::new("key-form-save")
                            .label(if form.saving {
                                "验证中…"
                            } else {
                                "保存"
                            })
                            .primary()
                            .compact()
                            .loading(form.saving)
                            .disabled(form.saving)
                            .on_click(cx.listener(|this, _, _, cx| this.save_key(cx))),
                    ),
            )
    }

    /// One key card: provider logo + label header, quota meters, model rows,
    /// status tags and row actions. `rests` are the key's active cooldowns (health info):
    /// one tag per row — a whole-key row or a per-model row — showing the
    /// recovery time, with a shared clear action. Dragging starts only from the
    /// grip dots in the top-left corner; the whole card stays a drop target —
    /// dropping on the left/right half of another card inserts before/after it.
    pub(super) fn key_card(
        &self,
        card_data: &KeyCardData,
        rests: &[RestRow],
        cx: &mut Context<Self>,
    ) -> Div {
        let provider = &card_data.provider;
        let key = &card_data.key;
        if let Some(editor) = self
            .key_editor
            .as_ref()
            .filter(|editor| editor.key_id == key.id)
        {
            return self.key_editor_card(editor, card_data, cx);
        }
        let provider_id = provider.id;
        let key_id = key.id;
        let enabled = key.enabled;
        let label = key.label.clone();
        let edit_key = key.clone();
        let spec = spec_for(&provider.name);

        // Drag-and-drop wiring: this card is both a drag source and a drop target.
        let weak = cx.weak_entity();
        let weak_drag = weak.clone();
        let weak_move = weak.clone();
        let weak_menu = weak.clone();
        let drag_label = key.label.clone();
        let (is_drag_source, insert_after) = match &self.drag {
            Some(drag) => (
                drag.dragged == key_id,
                match drag.target {
                    Some((target, after)) if target == key_id && drag.dragged != key_id => {
                        Some(after)
                    }
                    _ => None,
                },
            ),
            None => (false, None),
        };
        // Drag visuals only apply while a drag is actually active (gpui-fast is
        // retained-mode: this also keeps a stale leftover from sticking around).
        let active = cx.has_active_drag();
        let dim_source = active && is_drag_source;
        let insert_after = insert_after.filter(|_| active);

        card(cx)
      .group("key-card")
      .on_drag_move::<DraggedKey>(move |event, _window, cx| {
        // The drag *origin* card's move listener fires wherever the pointer is
        // ("as long as the drag started on this element"); only a card whose
        // bounds actually contain the pointer may claim target status.
        if !event.bounds.contains(&event.event.position) {
          return;
        }
        let after = event.event.position.x >= event.bounds.center().x;
        weak_move
          .update(cx, |this, cx| {
            let dragged = this.drag.as_ref().map(|drag| drag.dragged).unwrap_or_default();
            let target = Some((key_id, after));
            if this.drag.as_ref().map(|drag| drag.target) != Some(target) {
              this.drag = Some(KeyDrag { dragged, target });
              cx.notify();
            }
          })
          .ok();
      })
      .on_drop::<DraggedKey>(move |dropped, _window, cx| {
        weak
          .update(cx, |this, cx| {
            let Some(drag) = this.drag.take() else { return };
            let Some((target, after)) = drag.target else {
              cx.notify();
              return;
            };
            let LoadState::Loaded(data) = &this.state else {
              cx.notify();
              return;
            };
            let ids: Vec<i64> = data.keys.iter().map(|card| card.key.id).collect();
            let order = reordered(&ids, dropped.id, target, after);
            if order == ids {
              cx.notify(); // dropped on itself: clear the drag visuals
              return;
            }
            this.write(cx, move |admin| async move { admin.reorder_keys(&order).await });
          })
          .ok();
      })
      .relative()
      .w(px(KEY_CARD_WIDTH))
      .when(!key.enabled || !provider.enabled, |this| this.opacity(0.6))
      .when(dim_source, |this| this.opacity(0.4))
      // Insert indicator: a 2px accent line in the gap on the side where the
      // dragged card would land (left = before this card, right = after).
      .when_some(insert_after, |this, after| {
        this.child(
          gpui::div()
            .absolute()
            .top_0()
            .bottom_0()
            .w(px(2.))
            .bg(cx.theme().primary)
            .when(after, |this| this.right(px(-11.)))
            .when(!after, |this| this.left(px(-11.))),
        )
      })
      // Header: provider icon, key label, probe age, and the ⋯ menu holding
      // enable/edit/delete. The logo's position is fixed — the drag grip
      // lives in the card padding, left of it.
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
          // The quota reading's probe age lives on the title row (配额 header
          // row removed — the card is one row shorter).
          .when_some(card_data.quota.as_ref(), |this, quota| {
            this.child(
              gpui::div()
                .text_size(px(11.))
                .text_color(cx.theme().muted_foreground)
                .child(crate::fmt::probe_age(quota.probed_at, chrono::Utc::now())),
            )
          })
          .child(
            Button::new(("key-menu", key_id as u64))
              .icon(Icon::new(IconName::Ellipsis).small())
              .ghost()
              .compact()
              .dropdown_menu_with_anchor(Anchor::TopRight, {
                let weak_menu = weak_menu.clone();
                let log = move |msg: String| {
                  use std::io::Write as _;
                  if let Ok(mut file) = std::fs::OpenOptions::new().create(true).append(true).open("/tmp/cardinal/card-menu.log") {
                    let _ = writeln!(file, "{msg}");
                  }
                };
                move |menu, _, _| {
                  log(format!("[build] menu provider={provider_id} key={key_id} captured_enabled={enabled}"));
                  menu
                    .item(PopupMenuItem::new("启用").checked(enabled).on_click({
                      let weak = weak_menu.clone();
                      // listener_for-style plumbing (the library's own pattern):
                      // the handler carries the window itself, and Entity::update
                      // never consults the entity→window map — which has no entry
                      // for this page and made update_in fail with
                      // "entity has no current window".
                      move |_, _window, cx| {
                        // The menu entity is cached between opens, so the captured
                        // `enabled` can go stale — read the key's current state at
                        // click time; the captured value is only the fallback.
                        let read = weak.read_with(cx, |page, _| page.key_enabled(provider_id, key_id));
                        let target = match read {
                          Ok(Some(current)) => !current,
                          _ => !enabled,
                        };
                        if let Err(error) = weak.update(cx, |this, cx| {
                          this.write(cx, move |admin| async move {
                            admin.update_key(provider_id, key_id, None, Some(target), None, None).await
                          });
                        }) {
                          eprintln!("card menu enable failed: {error}");
                        }
                      }
                    }))
                    .separator()
                    .item(PopupMenuItem::new("编辑").icon(Icon::new(IconName::Pencil).small()).on_click({
                      let weak = weak_menu.clone();
                      let edit_key = edit_key.clone();
                      move |_, window, cx| {
                        if let Err(error) = weak.update(cx, |this, cx| this.open_key_editor(provider_id, &edit_key, window, cx)) {
                          eprintln!("card menu edit failed: {error}");
                        }
                      }
                    }))
                    .item(PopupMenuItem::new("删除").icon(Icon::new(IconName::Delete).small()).on_click({
                      let weak = weak_menu.clone();
                      let label = label.clone();
                      move |_, window, cx| {
                        if let Err(error) =
                          weak.update(cx, |this, cx| this.confirm_delete_key(provider_id, key_id, label.clone(), window, cx))
                        {
                          eprintln!("card menu delete failed: {error}");
                        }
                      }
                    }))
                }
              }),
          ),
      )
      // Variant markers (kimi/glm only), display-only on their own row under
      // the title: 区域 (国内/国际) + Coding Plan — the variant decides the
      // anthropic capability, so the card says which entry it sits on.
      // Non-catalog rows (a manually-added provider) render no markers.
      .when_some(variant_of_row(&provider.name, &provider.base_url), |this, (region, coding_plan)| {
        let marker = |text: &'static str, cx: &mut Context<Self>| {
          gpui::div()
            .rounded_md()
            .border_1()
            .px_1p5()
            .text_size(px(10.))
            .border_color(cx.theme().accent)
            .text_color(cx.theme().muted_foreground)
            .child(text)
        };
        let mut row = gpui::div().flex().items_center().gap_2().child(marker(region.label(), cx));
        if coding_plan {
          row = row.child(marker("Coding Plan", cx));
        }
        this.child(row)
      })
      // Quota: one meter per probed window, and/or a balance line. A key
      // without a reading (unknown vendor, probe never succeeded) renders
      // neither — the section collapses and the card shrinks.
      .when_some(card_data.quota.as_ref(), |this, quota| {
        let mut this = this;
        if !quota.windows.is_empty() {
          this = this.child(quota_row(&quota.windows, cx));
        }
        if let Some(balance) = &quota.balance {
          this = this.child(gpui::div().text_size(px(12.)).text_color(cx.theme().muted_foreground).child(format!("余额 {balance}")));
        }
        this
      })
      // Model rows: one line per whitelisted model (see `model_rows`), placed
      // between the quota meters and the footer.
      .child(model_rows(key, cx))
      // Footer: status tags + row actions.
      .child(
        gpui::div()
          .flex()
          .items_center()
          .gap_1()
          .pt_1()
          // One cooldown tag per rest row (whole-key rows omit the model); the
          // reason and the clear action (which wakes the whole key) appear once.
          .when(!rests.is_empty(), |this| {
            let mut this = this;
            for (index, rest) in rests.iter().enumerate() {
              // Same calendar day shows just the clock; anything further
              // (a monthly quota reset, an 8-day cap) must carry the date.
              let until = rest.rest_until.with_timezone(&crate::ui_tz::display_tz());
              let today = chrono::Utc::now()
                .with_timezone(&crate::ui_tz::display_tz())
                .date_naive();
              let until = if until.date_naive() == today {
                until.format("%H:%M").to_string()
              } else {
                until.format("%m-%d %H:%M").to_string()
              };
              let (tag, tip) = match &rest.model {
                Some(model) => (format!("冷却 {model} · {until} 恢复"), rest.reason.clone()),
                None => (format!("冷却 · {until} 恢复"), rest.reason.clone()),
              };
              // The reason is not shown inline — a long upstream message (e.g. a
              // monthly quota notice) would be truncated into noise. It lives on
              // the tag's tooltip, revealed on hover.
              let tag_el = match tip {
                Some(reason) if !reason.is_empty() => {
                  let reason = reason.clone();
                  gpui::div()
                    .id(("rest-tag", index as u64))
                    .child(Tag::warning().child(tag))
                    .tooltip(move |_, cx| cx.new(|_| StatusTip { text: reason.clone() }).into())
                }
                _ => gpui::div().id(("rest-tag", index as u64)).child(Tag::warning().child(tag)),
              };
              this = this.child(tag_el);
            }
            // Spacer pushes the clear button to the right edge of the footer row.
            this.child(gpui::div().flex_1()).child(Button::new(("rest-clear", key_id as u64)).label("清除").ghost().compact().on_click(
              cx.listener(move |this, _, _, cx| {
                this.write(cx, move |admin| async move { admin.clear_rest(key_id).await });
              }),
            ))
          })
          .when(rests.is_empty(), |this| this.child(gpui::div().flex_1())),
      )
      // Drag grip (three dots, single column): lives in the card's left
      // padding, vertically aligned with the header logo — the logo's position
      // never shifts. Hidden until hovered; the only grab cursor on the card.
      .child(
        gpui::div()
          .id(("key-grip", key_id as u64))
          .absolute()
          .top(px(14.))
          .left(px(3.5))
          .w(px(12.))
          .p_1()
          .opacity(0.)
          .group_hover("key-card", |this| this.opacity(0.5))
          .cursor_grab()
          .on_drag(DraggedKey { id: key_id }, move |_dragged, _offset, _window, cx| {
            weak_drag
              .update(cx, |this, cx| {
                this.drag = Some(KeyDrag { dragged: key_id, target: None });
                cx.notify();
              })
              .ok();
            cx.new(|_| DragGhost { label: drag_label.clone() })
          })
          .flex()
          .flex_col()
          .gap(px(3.))
          .children((0..3).map(|_| gpui::div().w(px(2.)).h(px(2.)).rounded_full().bg(cx.theme().muted_foreground).opacity(0.5))),
      )
    }

    /// The card's edit mode: label and concurrency-cap inputs, save/cancel. The
    /// API key itself is read-only (rotate = delete + re-add): masked by default,
    /// with a reveal toggle and a copy button. Its plaintext is fetched when the
    /// editor opens (the list shapes carry none): until that lands, or if it
    /// fails, the row shows a placeholder and the copy button is disabled.
    pub(super) fn key_editor_card(
        &self,
        editor: &KeyEditor,
        card_data: &KeyCardData,
        cx: &mut Context<Self>,
    ) -> Div {
        // None = the reveal has not landed (loading), or it failed. The editor
        // stays usable for label/concurrency/models meanwhile.
        let api_key: Option<String> = editor.api_key.clone();
        let copy_key = api_key.clone();
        let masked = api_key.as_deref().map(mask_api_key);
        let eye = if editor.show_key {
            IconName::EyeOff
        } else {
            IconName::Eye
        };
        card(cx)
            .w(px(KEY_CARD_WIDTH))
            .child(
                gpui::div()
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .text_color(cx.theme().foreground)
                    .child("编辑账号"),
            )
            .child(
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
            )
            .child(editor_field(
                "Label",
                Input::new(&editor.label),
                Some(160.),
                cx,
            ))
            .child(editor_field(
                "并发上限",
                Input::new(&editor.max_concurrency),
                Some(96.),
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
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.key_editor = None;
                                cx.notify();
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

/// Drag payload for card reordering (the value carried by an active drag).
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

/// The key card's model rows: one line per whitelisted model (`KEY_MODEL_ROWS`
/// shown, the rest folded into an 「其他模型 N 个」 hint), the key's concurrency
/// sitting after each model ID — a key-level metric repeated per row by
/// design. A key without a whitelist (= 全部模型) shows the concurrency alone,
/// so a limited key keeps its occupancy visible; with neither, the section
/// collapses.
fn model_rows(key: &ProviderKeyView, cx: &App) -> Div {
    let concurrency = key
        .max_concurrency
        .map(|max| format!("并发 {}/{}", key.inflight.unwrap_or(0), max));
    let mut rows: Vec<gpui::AnyElement> = Vec::new();
    match key
        .allowed_models
        .as_ref()
        .filter(|models| !models.is_empty())
    {
        Some(models) => {
            for model in models.iter().take(KEY_MODEL_ROWS) {
                let mut row = gpui::div().flex().items_center().gap_1().child(
                    gpui::div()
                        .flex_1()
                        .min_w(px(0.))
                        .overflow_hidden()
                        .text_ellipsis()
                        .whitespace_nowrap()
                        .text_size(px(12.))
                        .text_color(cx.theme().foreground)
                        .child(model.clone()),
                );
                if let Some(text) = &concurrency {
                    row = row.child(Tag::secondary().child(text.clone()));
                }
                rows.push(row.into_any_element());
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
        }
        None => {
            if let Some(text) = &concurrency {
                rows.push(Tag::secondary().child(text.clone()).into_any_element());
            }
        }
    }
    gpui::div().flex().flex_col().gap_1().children(rows)
}

/// Provider icon at a given render size: the embedded SVG logo, rasterized
/// full-color (gpui's `svg()` element renders monochrome alpha-masks, so
/// brand logos go through `img()` instead).
fn provider_icon(spec: &'static ProviderSpec, height: f32, _cx: &App) -> Div {
    let ProviderIcon::Svg(bytes) = &spec.icon;
    let image = Arc::new(Image::from_bytes(ImageFormat::Svg, bytes.to_vec()));
    gpui::div()
        .flex_shrink_0()
        .child(img(image).w(px(height)).h(px(height)))
}

/// Icon for an arbitrary provider row: catalog logo when known, else a letter
/// tile from the provider's stored name.
fn provider_icon_for(
    provider: &ProviderRow,
    spec: Option<&'static ProviderSpec>,
    height: f32,
    cx: &App,
) -> Div {
    match spec {
        Some(spec) => provider_icon(spec, height, cx),
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
/// editor): model id plus the attribute badges the probe list carried
/// (id · 上下文 · 输出上限 · 模态 · 推理), highlighted when selected. Same
/// render conventions as the route editor's model chips.
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
    for modality in &model.input_modalities {
        let badge = match modality.as_str() {
            "image" => "图",
            "audio" => "音",
            "video" => "视",
            other => other,
        };
        chip_text.push_str(&format!(" · {badge}"));
    }
    if !model.effort_levels.is_empty() {
        chip_text.push_str(" · 推理");
    }
    let chip = gpui::div()
        .id(id)
        .cursor_pointer()
        .border_1()
        .rounded_md()
        .px_2()
        .py_0p5()
        .text_size(px(12.))
        .child(chip_text);
    if selected {
        chip.border_color(cx.theme().accent)
            .bg(cx.theme().accent)
            .text_color(cx.theme().accent_foreground)
    } else {
        chip.border_color(cx.theme().border)
            .text_color(cx.theme().foreground)
            .hover(|this| this.bg(cx.theme().accent))
    }
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
fn editor_field(label: &'static str, input: Input, width: Option<f32>, cx: &App) -> Div {
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
fn inline_error(message: &str, cx: &App) -> Div {
    gpui::div()
        .text_size(px(12.))
        .text_color(cx.theme().danger)
        .child(message.to_string())
}
