//! Settings page: a read-only Runtime card describing how this GUI reaches
//! the gateway (the standalone `cardinal-gateway` process over loopback
//! HTTP, startup state), plus a 显示 card for the display timezone — the one
//! GUI-local setting, written back to the open config file's `[gui]` section.

use gpui::prelude::FluentBuilder as _;
use gpui::{
    AppContext as _, Context, Entity, IntoElement, ParentElement, Render, Styled, Window, px,
};
use gpui_kit::component::ActiveTheme;
use gpui_kit::component::button::Button;
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::tag::Tag;

use crate::pages::{card, page_frame};
use crate::runtime::GatewayRuntime;

/// Startup state of the gateway connection.
enum RuntimeStatus {
    Starting,
    Connected,
    Failed(String),
}

/// Application settings.
pub struct SettingsPage {
    status: RuntimeStatus,
    /// The 显示 timezone input (`±HH:MM`; empty = follow the system).
    timezone: Entity<InputState>,
    /// Write-back failure of the last save; shown under the 显示 card.
    save_error: Option<String>,
}

impl SettingsPage {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let ready = cx.global::<GatewayRuntime>().await_ready();
        cx.spawn(async move |this, cx| {
            let result = ready.await;
            this.update(cx, |page, cx| {
                page.status = match result {
                    Ok(()) => RuntimeStatus::Connected,
                    Err(error) => RuntimeStatus::Failed(error),
                };
                cx.notify();
            })
            .ok();
        })
        .detach();
        // Prefill from the currently effective value (empty = follow system).
        let prefilled = crate::ui_tz::configured_timezone().unwrap_or_default();
        let timezone = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("±HH:MM，如 +08:00（空 = 跟随系统）")
                .default_value(prefilled)
        });
        Self {
            status: RuntimeStatus::Starting,
            timezone,
            save_error: None,
        }
    }

    /// Save the timezone: parse the input, write it back to the open config
    /// file's `[gui].timezone`, and apply it immediately. A parse or write
    /// failure lands in `save_error`; an empty input clears the key.
    fn save_timezone(&mut self, cx: &mut Context<Self>) {
        let raw = self.timezone.read(cx).value().to_string();
        let offset = match crate::ui_tz::parse_timezone_input(&raw) {
            Ok(offset) => offset,
            Err(error) => {
                self.save_error = Some(error);
                cx.notify();
                return;
            }
        };
        let trimmed = raw.trim();
        let value = (!trimmed.is_empty()).then_some(trimmed);
        let outcome = match crate::load::config_path() {
            Some(path) => crate::ui_tz::save_timezone(&path, value),
            None => Err("无法确定打开的配置文件路径，无法保存".to_string()),
        };
        match outcome {
            Ok(()) => {
                crate::ui_tz::set_display_tz(offset);
                self.save_error = None;
            }
            Err(error) => self.save_error = Some(error),
        }
        cx.notify();
    }
}

impl Render for SettingsPage {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let gateway_addr = cx.global::<GatewayRuntime>().base_url().to_string();

        page_frame("", "", cx)
            .child(
                card(cx)
                    .child(
                        gpui::div()
                            .font_weight(gpui::FontWeight::MEDIUM)
                            .text_color(cx.theme().foreground)
                            .child("Runtime"),
                    )
                    .child(field_row(
                        "运行模式",
                        "HTTP 客户端（gateway 独立进程：cardinal-gateway）",
                        cx,
                    ))
                    .child(field_row("网关", &gateway_addr, cx))
                    .child(
                        gpui::div()
                            .flex()
                            .items_center()
                            .justify_between()
                            .gap_4()
                            .child(gpui::div().text_color(cx.theme().foreground).child("状态"))
                            .child(match &self.status {
                                RuntimeStatus::Starting => gpui::div()
                                    .text_color(cx.theme().muted_foreground)
                                    .child("连接中…")
                                    .into_any_element(),
                                RuntimeStatus::Connected => {
                                    Tag::success().child("已连接").into_any_element()
                                }
                                RuntimeStatus::Failed(error) => gpui::div()
                                    .flex()
                                    .flex_col()
                                    .items_end()
                                    .gap_1()
                                    .child(Tag::danger().child("未连接"))
                                    .child(
                                        gpui::div()
                                            .text_size(px(12.))
                                            .text_color(cx.theme().muted_foreground)
                                            .child(error.clone()),
                                    )
                                    .into_any_element(),
                            }),
                    ),
            )
            .child(
                card(cx)
                    .child(
                        gpui::div()
                            .font_weight(gpui::FontWeight::MEDIUM)
                            .text_color(cx.theme().foreground)
                            .child("显示"),
                    )
                    .child(
                        gpui::div()
                            .flex()
                            .items_center()
                            .justify_between()
                            .gap_4()
                            .child(
                                gpui::div()
                                    .text_color(cx.theme().foreground)
                                    .child("显示时区"),
                            )
                            .child(
                                gpui::div()
                                    .flex()
                                    .items_center()
                                    .gap_2()
                                    .child(Input::new(&self.timezone).w(px(240.)))
                                    .child(Button::new("settings-tz-save").label("保存").on_click(
                                        cx.listener(|this, _, _, cx| {
                                            this.save_timezone(cx);
                                        }),
                                    )),
                            ),
                    )
                    .when_some(self.save_error.as_ref(), |this, error| {
                        this.child(
                            gpui::div()
                                .text_size(px(12.))
                                .text_color(cx.theme().danger)
                                .child(error.clone()),
                        )
                    }),
            )
    }
}

/// One labeled read-only row: label on the left, value on the right.
fn field_row(label: &'static str, value: &str, cx: &gpui::App) -> gpui::Div {
    gpui::div()
        .flex()
        .items_center()
        .justify_between()
        .gap_4()
        .child(gpui::div().text_color(cx.theme().foreground).child(label))
        .child(
            gpui::div()
                .text_size(px(12.))
                .text_color(cx.theme().muted_foreground)
                .child(value.to_string()),
        )
}
