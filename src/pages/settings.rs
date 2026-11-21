//! Settings page: a read-only Runtime card, the GUI-local settings written to
//! the open config file's `[gui]` section (timezone, 销售模式), two cards
//! backed by the Gateway's PostgreSQL (邮件发送 and 第三方登录), and a read-only
//! 在线支付 card derived from the local Gateway config. The DB cards call the
//! admin API and take effect immediately; the others keep their local-TOML
//! behavior.

use gpui::prelude::FluentBuilder as _;
use gpui::{
    AnyWindowHandle, AppContext as _, Context, Entity, IntoElement, ParentElement, Render, Styled,
    Window, px,
};
use gpui_kit::component::ActiveTheme;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::switch::Switch;
use gpui_kit::component::tag::Tag;

use crate::pages::{notify_error, notify_success};
use crate::payment_status::PaymentStatusView;
use crate::portal_settings::{
    AuthProviderView, MailDraft, PortalSettingsView, ProviderDraft, mail_body, notice,
    provider_body, safe_secret_display, secret_status,
};
use crate::runtime::GatewayRuntime;

/// Startup state of the gateway connection.
enum RuntimeStatus {
    Starting,
    Connected,
    Failed(String),
}

/// Text inputs of the 邮件发送 card. Secret fields hold the mask returned by
/// the Gateway, never a plaintext secret read from the database.
struct MailFields {
    mail_url: Entity<InputState>,
    mail_token: Entity<InputState>,
    mail_from: Entity<InputState>,
    mail_origin: Entity<InputState>,
    smtp_host: Entity<InputState>,
    smtp_port: Entity<InputState>,
    smtp_username: Entity<InputState>,
    smtp_password: Entity<InputState>,
    smtp_tls: Entity<InputState>,
    smtp_from: Entity<InputState>,
}

/// One provider row: the Gateway's masked view plus its editable inputs.
#[derive(Clone)]
struct ProviderFields {
    view: AuthProviderView,
    enabled: bool,
    client_id: Entity<InputState>,
    client_secret: Entity<InputState>,
    redirect_uri: Entity<InputState>,
}

/// Application settings.
pub struct SettingsPage {
    status: RuntimeStatus,
    /// Read-only public Xiaodou fields from the local Gateway config.
    payment: PaymentStatusView,
    /// The 显示 timezone input (`±HH:MM`; empty = follow the system).
    timezone: Entity<InputState>,
    /// Write-back failure of the last save; shown under the 显示 card.
    save_error: Option<String>,
    /// Write-back failure of the last 销售模式 toggle; shown under the 业务 card.
    sales_error: Option<String>,
    /// Gateway DB mail card.
    mail: MailFields,
    /// SMTP selected instead of the default HTTP transport.
    mail_smtp: bool,
    mail_error: Option<String>,
    /// Gateway DB provider card.
    providers: Vec<ProviderFields>,
    provider_error: Option<String>,
    #[cfg(feature = "screenshot-test")]
    screenshot_page: usize,
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
        let mut page = Self {
            status: RuntimeStatus::Starting,
            payment: crate::payment_status::load_local(),
            timezone,
            save_error: None,
            sales_error: None,
            mail: MailFields {
                mail_url: text_field(window, cx, "https://… 邮件发送接口"),
                mail_token: text_field(window, cx, "Token（******** = 保持原值）"),
                mail_from: text_field(window, cx, "发件人地址"),
                mail_origin: text_field(window, cx, "门户站点 Origin"),
                smtp_host: text_field(window, cx, "SMTP 服务器地址"),
                smtp_port: text_field(window, cx, "587"),
                smtp_username: text_field(window, cx, "SMTP 用户名"),
                smtp_password: text_field(window, cx, "SMTP 密码（******** = 保持原值）"),
                smtp_tls: text_field(window, cx, "none / starttls / tls"),
                smtp_from: text_field(window, cx, "SMTP 发件人地址"),
            },
            mail_smtp: false,
            mail_error: None,
            providers: Vec::new(),
            provider_error: None,
            #[cfg(feature = "screenshot-test")]
            screenshot_page: 0,
        };
        page.load_portal(window.window_handle(), cx);
        page
    }

    #[cfg(feature = "screenshot-test")]
    pub(crate) fn set_screenshot_page(&mut self, page: usize, cx: &mut Context<Self>) {
        self.screenshot_page = page;
        cx.notify();
    }

    /// Load both DB-backed cards from the Gateway. The returned views carry
    /// masked secrets only; the inputs are prefilled from them.
    fn load_portal(&mut self, handle: AnyWindowHandle, cx: &mut Context<Self>) {
        let Some(api) = cx.global::<GatewayRuntime>().portal() else {
            let error = "Gateway 管理 API 客户端不可用".to_owned();
            self.mail_error = Some(error.clone());
            self.provider_error = Some(error);
            cx.notify();
            return;
        };
        let settings_call = cx.global::<GatewayRuntime>().admin_call({
            let api = api.clone();
            move |_| {
                let api = api.clone();
                async move { api.settings().await }
            }
        });
        let providers_call = cx
            .global::<GatewayRuntime>()
            .admin_call(move |_| async move { api.providers().await });
        let page = cx.entity();
        cx.spawn(async move |_, cx| {
            let settings = settings_call.await;
            let providers = providers_call.await;
            let _ = handle.update(cx, |_, window, cx| {
                page.update(cx, |page, cx| {
                    page.apply_mail(settings, window, cx);
                    page.apply_providers(providers, window, cx);
                    cx.notify();
                });
            });
        })
        .detach();
    }

    fn apply_mail(
        &mut self,
        result: Result<PortalSettingsView, String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match result {
            Ok(view) => {
                self.mail_smtp = view.mail_transport == "smtp";
                self.mail
                    .mail_url
                    .update(cx, |input, cx| input.set_value(&view.mail_url, window, cx));
                self.mail.mail_token.update(cx, |input, cx| {
                    input.set_value(safe_secret_display(&view.mail_token), window, cx)
                });
                self.mail
                    .mail_from
                    .update(cx, |input, cx| input.set_value(&view.mail_from, window, cx));
                self.mail.mail_origin.update(cx, |input, cx| {
                    input.set_value(&view.mail_origin, window, cx)
                });
                self.mail
                    .smtp_host
                    .update(cx, |input, cx| input.set_value(&view.smtp_host, window, cx));
                self.mail.smtp_port.update(cx, |input, cx| {
                    input.set_value(view.smtp_port.to_string(), window, cx)
                });
                self.mail.smtp_username.update(cx, |input, cx| {
                    input.set_value(&view.smtp_username, window, cx)
                });
                self.mail.smtp_password.update(cx, |input, cx| {
                    input.set_value(safe_secret_display(&view.smtp_password), window, cx)
                });
                self.mail
                    .smtp_tls
                    .update(cx, |input, cx| input.set_value(&view.smtp_tls, window, cx));
                self.mail
                    .smtp_from
                    .update(cx, |input, cx| input.set_value(&view.smtp_from, window, cx));
                self.mail_error = None;
            }
            Err(error) => self.mail_error = Some(error),
        }
    }

    fn apply_providers(
        &mut self,
        result: Result<Vec<AuthProviderView>, String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match result {
            Ok(views) => {
                self.providers = views
                    .into_iter()
                    .map(|view| ProviderFields {
                        enabled: view.enabled,
                        client_id: text_field_value(window, cx, "client_id", &view.client_id),
                        client_secret: text_field_value(
                            window,
                            cx,
                            "client_secret（******** = 保持原值）",
                            safe_secret_display(&view.client_secret),
                        ),
                        redirect_uri: text_field_value(
                            window,
                            cx,
                            "https://…/callback",
                            &view.redirect_uri,
                        ),
                        view,
                    })
                    .collect();
                self.provider_error = None;
            }
            Err(error) => self.provider_error = Some(error),
        }
    }

    /// Save the timezone: parse the input, write it back to the open config
    /// file's `[gui].timezone`, and apply it immediately. A parse or write
    /// failure lands in `save_error`; an empty input clears the key.
    fn save_timezone(&mut self, handle: AnyWindowHandle, cx: &mut Context<Self>) {
        let raw = self.timezone.read(cx).value().to_string();
        let offset = match crate::ui_tz::parse_timezone_input(&raw) {
            Ok(offset) => offset,
            Err(error) => {
                self.save_error = Some(error.clone());
                notify_error(handle, cx, format!("保存时区失败：{error}"));
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
                notify_success(handle, cx, "保存时区成功");
            }
            Err(error) => {
                self.save_error = Some(error.clone());
                notify_error(handle, cx, format!("保存时区失败：{error}"));
            }
        }
        cx.notify();
    }

    /// Toggle 销售模式: write it to the open config file, then apply it. A write
    /// failure keeps the old value — the switch must never claim a state the file
    /// does not hold, or the next start would silently disagree with what the card
    /// showed.
    fn set_sales_mode(&mut self, on: bool, handle: AnyWindowHandle, cx: &mut Context<Self>) {
        let outcome = match crate::load::config_path() {
            Some(path) => crate::sales_mode::save(&path, on),
            None => Err("无法确定打开的配置文件路径，无法保存".to_string()),
        };
        match outcome {
            Ok(()) => {
                crate::sales_mode::set_enabled(on);
                self.sales_error = None;
                notify_success(handle, cx, "切换销售模式成功");
            }
            Err(error) => {
                self.sales_error = Some(error.clone());
                notify_error(handle, cx, format!("切换销售模式失败：{error}"));
            }
        }
        cx.notify();
    }

    fn mail_draft(&self, cx: &gpui::App) -> MailDraft {
        MailDraft {
            is_smtp: self.mail_smtp,
            mail_url: self.mail.mail_url.read(cx).value().to_string(),
            mail_token: self.mail.mail_token.read(cx).value().to_string(),
            mail_from: self.mail.mail_from.read(cx).value().to_string(),
            mail_origin: self.mail.mail_origin.read(cx).value().to_string(),
            smtp_host: self.mail.smtp_host.read(cx).value().to_string(),
            smtp_port: self.mail.smtp_port.read(cx).value().to_string(),
            smtp_username: self.mail.smtp_username.read(cx).value().to_string(),
            smtp_password: self.mail.smtp_password.read(cx).value().to_string(),
            smtp_tls: self.mail.smtp_tls.read(cx).value().to_string(),
            smtp_from: self.mail.smtp_from.read(cx).value().to_string(),
        }
    }

    fn save_mail(&mut self, handle: AnyWindowHandle, cx: &mut Context<Self>) {
        let Some(api) = cx.global::<GatewayRuntime>().portal() else {
            self.mail_error = Some("Gateway 管理 API 客户端不可用".to_owned());
            notify_error(
                handle,
                cx,
                "保存邮件发送失败：Gateway 管理 API 客户端不可用",
            );
            cx.notify();
            return;
        };
        let body = match mail_body(&self.mail_draft(cx)) {
            Ok(body) => body,
            Err(error) => {
                self.mail_error = Some(error.clone());
                let (_, message) = notice("保存邮件发送", Err(error));
                notify_error(handle, cx, message);
                cx.notify();
                return;
            }
        };
        let call = cx.global::<GatewayRuntime>().admin_call({
            let api = api.clone();
            let body = body.clone();
            move |_| {
                let api = api.clone();
                let body = body.clone();
                async move { api.put_settings(&body).await }
            }
        });
        let page = cx.entity();
        cx.spawn(async move |_, cx| {
            let result = call.await;
            let _ = handle.update(cx, |_, window, cx| {
                page.update(cx, |page, cx| match result {
                    Ok(view) => {
                        page.apply_mail(Ok(view), window, cx);
                        let (_, message) = notice("保存邮件发送", Ok(()));
                        notify_success(handle, cx, message);
                        cx.notify();
                    }
                    Err(error) => {
                        page.apply_mail(Err(error.clone()), window, cx);
                        let (_, message) = notice("保存邮件发送", Err(error));
                        notify_error(handle, cx, message);
                        cx.notify();
                    }
                });
            });
        })
        .detach();
    }

    fn provider_draft(&self, index: usize, cx: &gpui::App) -> ProviderDraft {
        let row = &self.providers[index];
        ProviderDraft {
            provider: row.view.provider.clone(),
            client_id: row.client_id.read(cx).value().to_string(),
            client_secret: row.client_secret.read(cx).value().to_string(),
            redirect_uri: row.redirect_uri.read(cx).value().to_string(),
            enabled: row.enabled,
            extra: row.view.extra.clone(),
        }
    }

    fn save_provider(&mut self, provider: String, handle: AnyWindowHandle, cx: &mut Context<Self>) {
        let Some(index) = self
            .providers
            .iter()
            .position(|row| row.view.provider == provider)
        else {
            return;
        };
        let Some(api) = cx.global::<GatewayRuntime>().portal() else {
            self.provider_error = Some("Gateway 管理 API 客户端不可用".to_owned());
            notify_error(
                handle,
                cx,
                "保存第三方登录失败：Gateway 管理 API 客户端不可用",
            );
            cx.notify();
            return;
        };
        let body = match provider_body(&self.provider_draft(index, cx)) {
            Ok(body) => body,
            Err(error) => {
                self.provider_error = Some(error.clone());
                let (_, message) = notice("保存第三方登录", Err(error));
                notify_error(handle, cx, message);
                cx.notify();
                return;
            }
        };
        let call = cx.global::<GatewayRuntime>().admin_call({
            let api = api.clone();
            let provider = provider.clone();
            move |_| {
                let api = api.clone();
                let provider = provider.clone();
                let body = body.clone();
                async move { api.put_provider(&provider, &body).await }
            }
        });
        let page = cx.entity();
        cx.spawn(async move |_, cx| {
            let result = call.await;
            let _ = handle.update(cx, |_, window, cx| {
                page.update(cx, |page, cx| match result {
                    Ok(view) => {
                        if let Some(row) = page
                            .providers
                            .iter_mut()
                            .find(|row| row.view.provider == view.provider)
                        {
                            row.enabled = view.enabled;
                            row.view = view;
                            row.client_secret.update(cx, |input, cx| {
                                input.set_value(crate::portal_settings::SECRET_MASK, window, cx)
                            });
                        }
                        page.provider_error = None;
                        let (_, message) = notice("保存第三方登录", Ok(()));
                        notify_success(handle, cx, message);
                        page.load_portal(handle, cx);
                        cx.notify();
                    }
                    Err(error) => {
                        if let Some(row) = page
                            .providers
                            .iter_mut()
                            .find(|row| row.view.provider == provider)
                        {
                            row.enabled = row.view.enabled;
                        }
                        page.provider_error = Some(error.clone());
                        let (_, message) = notice("保存第三方登录", Err(error));
                        notify_error(handle, cx, message);
                        cx.notify();
                    }
                });
            });
        })
        .detach();
    }

    fn delete_provider(
        &mut self,
        provider: String,
        handle: AnyWindowHandle,
        cx: &mut Context<Self>,
    ) {
        let Some(api) = cx.global::<GatewayRuntime>().portal() else {
            self.provider_error = Some("Gateway 管理 API 客户端不可用".to_owned());
            notify_error(
                handle,
                cx,
                "删除第三方登录失败：Gateway 管理 API 客户端不可用",
            );
            cx.notify();
            return;
        };
        let call = cx.global::<GatewayRuntime>().admin_call({
            let api = api.clone();
            let provider = provider.clone();
            move |_| {
                let api = api.clone();
                let provider = provider.clone();
                async move { api.delete_provider(&provider).await }
            }
        });
        let page = cx.entity();
        cx.spawn(async move |_, cx| {
            let result = call.await;
            let _ = handle.update(cx, |_, _, cx| {
                page.update(cx, |page, cx| match result {
                    Ok(()) => {
                        let (_, message) = notice("删除第三方登录", Ok(()));
                        notify_success(handle, cx, message);
                        page.load_portal(handle, cx);
                        cx.notify();
                    }
                    Err(error) => {
                        page.provider_error = Some(error.clone());
                        let (_, message) = notice("删除第三方登录", Err(error));
                        notify_error(handle, cx, message);
                        cx.notify();
                    }
                });
            });
        })
        .detach();
    }
}

fn general_page(page: Entity<SettingsPage>) -> gpui_kit::component::setting::SettingPage {
    use gpui_kit::component::setting::{SettingGroup, SettingItem, SettingPage};

    SettingPage::new("常规")
        .description("运行状态、显示和业务偏好")
        .groups([
            SettingGroup::new()
                .title("运行状态")
                .description("当前 GUI 与独立 Gateway 的连接信息")
                .variant(gpui_kit::component::group_box::GroupBoxVariant::Outline)
                .item(SettingItem::render({
                    let page = page.clone();
                    move |_, _, cx| {
                        let page = page.read(cx);
                        let gateway_addr = cx.global::<GatewayRuntime>().base_url().to_string();
                        let status = match &page.status {
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
                        };
                        gpui::div()
                            .flex()
                            .flex_col()
                            .gap_3()
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
                                    .child(
                                        gpui::div().text_color(cx.theme().foreground).child("状态"),
                                    )
                                    .child(status),
                            )
                    }
                })),
            SettingGroup::new()
                .title("显示")
                .variant(gpui_kit::component::group_box::GroupBoxVariant::Outline)
                .item(SettingItem::render({
                    let page = page.clone();
                    move |_, _, cx| {
                        let timezone = page.read(cx).timezone.clone();
                        let save_error = page.read(cx).save_error.clone();
                        let page_for_save = page.clone();
                        gpui::div()
                            .flex()
                            .flex_col()
                            .gap_2()
                            .child(
                                gpui::div()
                                    .flex()
                                    .items_center()
                                    .justify_between()
                                    .gap_4()
                                    .child(
                                        gpui::div()
                                            .flex()
                                            .flex_col()
                                            .gap_0p5()
                                            .child(
                                                gpui::div()
                                                    .text_color(cx.theme().foreground)
                                                    .child("显示时区"),
                                            )
                                            .child(
                                                gpui::div()
                                                    .text_size(px(12.))
                                                    .text_color(cx.theme().muted_foreground)
                                                    .child("空值表示跟随系统"),
                                            ),
                                    )
                                    .child(
                                        gpui::div()
                                            .flex()
                                            .items_center()
                                            .gap_2()
                                            .child(Input::new(&timezone).w(px(240.)))
                                            .child(
                                                Button::new("settings-tz-save")
                                                    .label("保存")
                                                    .on_click(move |_, window, cx| {
                                                        page_for_save.update(cx, |page, cx| {
                                                            page.save_timezone(
                                                                window.window_handle(),
                                                                cx,
                                                            )
                                                        });
                                                    }),
                                            ),
                                    ),
                            )
                            .when_some(save_error, |this, error| {
                                this.child(
                                    gpui::div()
                                        .text_size(px(12.))
                                        .text_color(cx.theme().danger)
                                        .child(error),
                                )
                            })
                    }
                })),
            SettingGroup::new()
                .title("业务")
                .variant(gpui_kit::component::group_box::GroupBoxVariant::Outline)
                .item(SettingItem::render({
                    let page = page.clone();
                    move |_, _, cx| {
                        let sales_error = page.read(cx).sales_error.clone();
                        let page_for_toggle = page.clone();
                        gpui::div()
                            .flex()
                            .flex_col()
                            .gap_2()
                            .child(
                                gpui::div()
                                    .flex()
                                    .items_center()
                                    .justify_between()
                                    .gap_4()
                                    .child(
                                        gpui::div()
                                            .flex()
                                            .flex_col()
                                            .gap_0p5()
                                            .child(
                                                gpui::div()
                                                    .text_color(cx.theme().foreground)
                                                    .child("销售模式"),
                                            )
                                            .child(
                                                gpui::div()
                                                    .text_size(px(12.))
                                                    .text_color(cx.theme().muted_foreground)
                                                    .child("关闭后概览页不显示营收"),
                                            ),
                                    )
                                    .child(
                                        Switch::new("settings-sales-mode")
                                            .checked(crate::sales_mode::enabled())
                                            .on_change(move |on: &bool, window, cx| {
                                                page_for_toggle.update(cx, |page, cx| {
                                                    page.set_sales_mode(
                                                        *on,
                                                        window.window_handle(),
                                                        cx,
                                                    )
                                                });
                                            }),
                                    ),
                            )
                            .when_some(sales_error, |this, error| {
                                this.child(
                                    gpui::div()
                                        .text_size(px(12.))
                                        .text_color(cx.theme().danger)
                                        .child(error),
                                )
                            })
                    }
                })),
        ])
}

fn mail_page(page: Entity<SettingsPage>) -> gpui_kit::component::setting::SettingPage {
    use gpui_kit::component::setting::{SettingGroup, SettingItem, SettingPage};

    SettingPage::new("邮件发送")
        .description("配置 Gateway 的邮件发送方式")
        .group(
            SettingGroup::new()
                .title("邮件服务")
                .description("写入 Gateway 的 PostgreSQL，保存后立即生效")
                .variant(gpui_kit::component::group_box::GroupBoxVariant::Outline)
                .item(SettingItem::render(move |_, _, cx| {
                    let page_state = page.read(cx);
                    let mail_smtp = page_state.mail_smtp;
                    let mail_url = page_state.mail.mail_url.clone();
                    let mail_token = page_state.mail.mail_token.clone();
                    let mail_from = page_state.mail.mail_from.clone();
                    let mail_origin = page_state.mail.mail_origin.clone();
                    let smtp_host = page_state.mail.smtp_host.clone();
                    let smtp_port = page_state.mail.smtp_port.clone();
                    let smtp_username = page_state.mail.smtp_username.clone();
                    let smtp_password = page_state.mail.smtp_password.clone();
                    let smtp_tls = page_state.mail.smtp_tls.clone();
                    let smtp_from = page_state.mail.smtp_from.clone();
                    let mail_error = page_state.mail_error.clone();
                    let _ = page_state;
                    let page_for_transport = page.clone();
                    let page_for_refresh = page.clone();
                    let page_for_save = page.clone();
                    let secret_summary = format!(
                        "HTTP {} · SMTP {}",
                        secret_status(mail_token.read(cx).value().as_str()),
                        secret_status(smtp_password.read(cx).value().as_str())
                    );
                    gpui::div()
                        .flex()
                        .flex_col()
                        .gap_3()
                        .child(
                            gpui::div()
                                .flex()
                                .items_center()
                                .justify_between()
                                .gap_4()
                                .child(
                                    gpui::div()
                                        .text_color(cx.theme().foreground)
                                        .child("传输方式"),
                                )
                                .child(
                                    gpui::div()
                                        .flex()
                                        .items_center()
                                        .gap_2()
                                        .child(
                                            gpui::div()
                                                .text_color(if mail_smtp {
                                                    cx.theme().muted_foreground
                                                } else {
                                                    cx.theme().primary
                                                })
                                                .child("HTTP"),
                                        )
                                        .child(
                                            Switch::new("settings-mail-transport")
                                                .checked(mail_smtp)
                                                .on_change(move |on: &bool, _, cx| {
                                                    page_for_transport.update(cx, |page, cx| {
                                                        page.mail_smtp = *on;
                                                        cx.notify();
                                                    });
                                                }),
                                        )
                                        .child(
                                            gpui::div()
                                                .text_color(if mail_smtp {
                                                    cx.theme().primary
                                                } else {
                                                    cx.theme().muted_foreground
                                                })
                                                .child("SMTP"),
                                        ),
                                ),
                        )
                        .when(!mail_smtp, |this| {
                            this.child(input_row("HTTP URL", &mail_url, cx))
                                .child(input_row("HTTP Token（掩码=保持）", &mail_token, cx))
                                .child(input_row("发件人", &mail_from, cx))
                                .child(input_row("站点 Origin", &mail_origin, cx))
                        })
                        .when(mail_smtp, |this| {
                            this.child(input_row("SMTP 服务器", &smtp_host, cx))
                                .child(input_row("SMTP 端口", &smtp_port, cx))
                                .child(input_row("SMTP 用户名", &smtp_username, cx))
                                .child(input_row("SMTP 密码（掩码=保持）", &smtp_password, cx))
                                .child(input_row("SMTP 加密", &smtp_tls, cx))
                                .child(input_row("SMTP 发件人", &smtp_from, cx))
                        })
                        .child(field_row("密钥状态", &secret_summary, cx))
                        .child(
                            gpui::div()
                                .flex()
                                .justify_end()
                                .gap_2()
                                .child(
                                    Button::new("settings-portal-refresh")
                                        .label("重新读取")
                                        .outline()
                                        .compact()
                                        .on_click(move |_, window, cx| {
                                            page_for_refresh.update(cx, |page, cx| {
                                                page.load_portal(window.window_handle(), cx)
                                            });
                                        }),
                                )
                                .child(
                                    Button::new("settings-mail-save")
                                        .label("保存邮件设置")
                                        .on_click(move |_, window, cx| {
                                            page_for_save.update(cx, |page, cx| {
                                                page.save_mail(window.window_handle(), cx)
                                            });
                                        }),
                                ),
                        )
                        .when_some(mail_error, |this, error| {
                            this.child(
                                gpui::div()
                                    .text_size(px(12.))
                                    .text_color(cx.theme().danger)
                                    .child(error),
                            )
                        })
                })),
        )
}

fn providers_page(page: Entity<SettingsPage>) -> gpui_kit::component::setting::SettingPage {
    use gpui_kit::component::setting::{SettingGroup, SettingItem, SettingPage};

    SettingPage::new("第三方登录")
        .description("管理 Gateway 中保存的 OAuth provider")
        .group(
            SettingGroup::new()
                .title("登录 provider")
                .description("client_secret 不回显，掩码表示保持原值，留空表示清空")
                .variant(gpui_kit::component::group_box::GroupBoxVariant::Outline)
                .item(SettingItem::render(move |_, _, cx| {
                    let (provider_rows, empty, error) = {
                        let page_state = page.read(cx);
                        (
                            page_state.providers.clone(),
                            page_state.providers.is_empty(),
                            page_state.provider_error.clone(),
                        )
                    };
                    let rows = provider_rows
                        .iter()
                        .enumerate()
                        .map(|(index, row)| provider_row(index, row, page.clone(), cx))
                        .collect::<Vec<_>>();
                    let mut content = gpui::div().flex().flex_col().gap_3();
                    for row in rows {
                        content = content.child(row);
                    }
                    content
                        .when(empty, |this| {
                            this.child(
                                gpui::div()
                                    .text_size(px(12.))
                                    .text_color(cx.theme().muted_foreground)
                                    .child("Gateway 数据库中没有已配置的第三方登录 provider。"),
                            )
                        })
                        .when_some(error, |this, error| {
                            this.child(
                                gpui::div()
                                    .text_size(px(12.))
                                    .text_color(cx.theme().danger)
                                    .child(error),
                            )
                        })
                })),
        )
}

fn payment_page(page: Entity<SettingsPage>) -> gpui_kit::component::setting::SettingPage {
    use gpui_kit::component::setting::{SettingGroup, SettingItem, SettingPage};

    SettingPage::new("在线支付")
        .description("只读展示本地 Gateway 配置的公开字段")
        .group(
            SettingGroup::new()
                .title("支付配置")
                .description("RSA 私钥由 Gateway 环境变量注入，GUI 不保存也不显示")
                .variant(gpui_kit::component::group_box::GroupBoxVariant::Outline)
                .item(SettingItem::render(move |_, _, cx| {
                    let rows = crate::payment_status::card_rows(&page.read(cx).payment);
                    let mut content = gpui::div().flex().flex_col().gap_3();
                    for (label, value) in rows {
                        content = content.child(field_row(label, &value, cx));
                    }
                    content
                })),
        )
}

fn provider_row(
    index: usize,
    row: &ProviderFields,
    page: Entity<SettingsPage>,
    cx: &mut gpui::App,
) -> gpui::Div {
    let provider = row.view.provider.clone();
    let provider_for_save = provider.clone();
    let provider_for_delete = provider.clone();
    let provider_for_toggle = provider.clone();
    let page_for_toggle = page.clone();
    gpui::div()
        .flex()
        .flex_col()
        .gap_2()
        .rounded_md()
        .border_1()
        .border_color(cx.theme().border)
        .p_3()
        .child(
            gpui::div()
                .flex()
                .items_center()
                .justify_between()
                .gap_4()
                .child(
                    gpui::div()
                        .font_weight(gpui::FontWeight::MEDIUM)
                        .text_color(cx.theme().foreground)
                        .child(format!(
                            "{} · 密钥 {}",
                            provider,
                            secret_status(&row.view.client_secret)
                        )),
                )
                .child(
                    gpui::div()
                        .flex()
                        .items_center()
                        .gap_2()
                        .child(
                            gpui::div()
                                .text_color(cx.theme().muted_foreground)
                                .child("启用"),
                        )
                        .child(
                            Switch::new(format!("settings-provider-enabled-{index}"))
                                .checked(row.enabled)
                                .on_change(move |on: &bool, _, cx| {
                                    let provider = provider_for_toggle.clone();
                                    page_for_toggle.update(cx, |page, cx| {
                                        if let Some(row) = page
                                            .providers
                                            .iter_mut()
                                            .find(|row| row.view.provider == provider)
                                        {
                                            row.enabled = *on;
                                        }
                                        cx.notify();
                                    });
                                }),
                        ),
                ),
        )
        .child(input_row("client_id", &row.client_id, cx))
        .child(input_row("client_secret", &row.client_secret, cx))
        .child(input_row("回调地址", &row.redirect_uri, cx))
        .child(
            gpui::div()
                .flex()
                .justify_end()
                .gap_2()
                .child(
                    Button::new(format!("settings-provider-save-{index}"))
                        .label("保存")
                        .on_click({
                            let page = page.clone();
                            move |_, window, cx| {
                                page.update(cx, |page, cx| {
                                    page.save_provider(
                                        provider_for_save.clone(),
                                        window.window_handle(),
                                        cx,
                                    )
                                });
                            }
                        }),
                )
                .child(
                    Button::new(format!("settings-provider-delete-{index}"))
                        .label("删除")
                        .danger()
                        .on_click(move |_, window, cx| {
                            page.update(cx, |page, cx| {
                                page.delete_provider(
                                    provider_for_delete.clone(),
                                    window.window_handle(),
                                    cx,
                                )
                            });
                        }),
                ),
        )
}

impl Render for SettingsPage {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let page = cx.entity();
        gpui::div().size_full().min_w_0().child(
            gpui_kit::component::setting::Settings::new(format!("settings-page-{}", {
                #[cfg(feature = "screenshot-test")]
                {
                    self.screenshot_page
                }
                #[cfg(not(feature = "screenshot-test"))]
                {
                    0
                }
            },))
            .sidebar_width(px(220.))
            .sidebar_size_range(px(180.)..px(320.))
            .with_group_variant(gpui_kit::component::group_box::GroupBoxVariant::Outline)
            .default_selected_index(gpui_kit::component::setting::SelectIndex {
                page_ix: {
                    #[cfg(feature = "screenshot-test")]
                    {
                        self.screenshot_page
                    }
                    #[cfg(not(feature = "screenshot-test"))]
                    {
                        0
                    }
                },
                group_ix: None,
            })
            .pages([
                general_page(page.clone()),
                mail_page(page.clone()),
                providers_page(page.clone()),
                payment_page(page),
            ]),
        )
    }
}

/// One labeled input row.
fn input_row(label: &'static str, input: &Entity<InputState>, cx: &gpui::App) -> gpui::Div {
    gpui::div()
        .flex()
        .items_center()
        .justify_between()
        .gap_4()
        .child(gpui::div().text_color(cx.theme().foreground).child(label))
        .child(Input::new(input).w(px(320.)))
}

fn text_field(
    window: &mut Window,
    cx: &mut gpui::App,
    placeholder: &'static str,
) -> Entity<InputState> {
    cx.new(|cx| InputState::new(window, cx).placeholder(placeholder))
}

fn text_field_value(
    window: &mut Window,
    cx: &mut gpui::App,
    placeholder: &'static str,
    value: &str,
) -> Entity<InputState> {
    cx.new(|cx| {
        InputState::new(window, cx)
            .placeholder(placeholder)
            .default_value(value)
    })
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
