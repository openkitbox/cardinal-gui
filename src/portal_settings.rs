//! GUI side of the Gateway's DB-backed portal configuration: wire shapes, form
//! validation and the admin API client.
//!
//! The two cards on the settings page are deliberately unlike the timezone and
//! 销售模式 cards: those write the local TOML, these write PostgreSQL through
//! the Gateway's admin API and take effect immediately. Secrets travel in one
//! direction only — the Gateway never sends plaintext back, and a `********`
//! value on a write means "keep the stored secret".

use std::sync::Arc;
use std::time::Duration;

use gateway::http_client::ClientError;
use serde::{Deserialize, Serialize};

/// Fixed mask used by the Gateway for an already-configured secret.
pub(crate) const SECRET_MASK: &str = "********";

const SETTINGS_PATH: &str = "/api/portal/settings";
const PROVIDERS_PATH: &str = "/api/portal/providers";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// Masked admin view of `portal_settings`. `updated_at` is intentionally not
/// modelled: the cards show the values and the save result, not row metadata.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub(crate) struct PortalSettingsView {
    pub mail_transport: String,
    pub mail_url: String,
    pub mail_token: String,
    pub mail_from: String,
    pub mail_origin: String,
    pub smtp_host: String,
    pub smtp_port: i32,
    pub smtp_username: String,
    pub smtp_password: String,
    pub smtp_tls: String,
    pub smtp_from: String,
}

/// Full replacement body of `PUT /api/portal/settings`.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub(crate) struct PortalSettingsBody {
    pub mail_transport: String,
    pub mail_url: String,
    pub mail_token: String,
    pub mail_from: String,
    pub mail_origin: String,
    pub smtp_host: String,
    pub smtp_port: i32,
    pub smtp_username: String,
    pub smtp_password: String,
    pub smtp_tls: String,
    pub smtp_from: String,
}

/// Masked admin view of one login provider.
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub(crate) struct AuthProviderView {
    pub provider: String,
    pub client_id: String,
    pub client_secret: String,
    pub redirect_uri: String,
    pub enabled: bool,
    pub extra: serde_json::Value,
}

/// Body of `PUT /api/portal/providers/{provider}`.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub(crate) struct AuthProviderBody {
    pub client_id: String,
    pub client_secret: String,
    pub redirect_uri: String,
    pub enabled: bool,
    pub extra: serde_json::Value,
}

/// Owned input snapshot of the mail card, before validation and trimming.
#[derive(Debug, Clone)]
pub(crate) struct MailDraft {
    pub is_smtp: bool,
    pub mail_url: String,
    pub mail_token: String,
    pub mail_from: String,
    pub mail_origin: String,
    pub smtp_host: String,
    pub smtp_port: String,
    pub smtp_username: String,
    pub smtp_password: String,
    pub smtp_tls: String,
    pub smtp_from: String,
}

/// Owned input snapshot of one provider row.
#[derive(Debug, Clone)]
pub(crate) struct ProviderDraft {
    pub provider: String,
    pub client_id: String,
    pub client_secret: String,
    pub redirect_uri: String,
    pub enabled: bool,
    pub extra: serde_json::Value,
}

/// A secret input's wire value: the mask keeps the stored value, empty clears.
#[must_use]
pub(crate) fn secret_wire_value(raw: &str) -> String {
    let trimmed = raw.trim();
    if trimmed == SECRET_MASK {
        SECRET_MASK.to_owned()
    } else {
        trimmed.to_owned()
    }
}

/// What a secret field may safely render. Even a leaked plaintext value from a
/// drifting Gateway becomes a mask before it reaches a text element.
#[must_use]
pub(crate) fn safe_secret_display(raw: &str) -> &'static str {
    if raw.is_empty() { "" } else { SECRET_MASK }
}

/// Short status line for a secret, used by the cards' rendered text.
#[must_use]
pub(crate) fn secret_status(raw: &str) -> &'static str {
    if raw.is_empty() {
        "未设置"
    } else {
        "已配置（********）"
    }
}

/// Labels that mark the two cards as DB-backed rather than local TOML.
#[must_use]
pub(crate) const fn card_labels() -> [&'static str; 2] {
    [
        "邮件发送（Gateway 数据库，保存后立即生效；非本地 TOML）",
        "第三方登录（Gateway 数据库，保存后立即生效；非本地 TOML）",
    ]
}

/// Build the mail body, validating every enum and the selected transport's
/// required fields. Returns a display-ready message on any bad input.
pub(crate) fn mail_body(draft: &MailDraft) -> Result<PortalSettingsBody, String> {
    let transport = if draft.is_smtp { "smtp" } else { "http" };
    if !["http", "smtp"].contains(&transport) {
        return Err("未知的邮件传输方式".to_owned());
    }
    let smtp_tls = draft.smtp_tls.trim();
    if !["none", "starttls", "tls"].contains(&smtp_tls) {
        return Err("SMTP 加密方式必须是 none / starttls / tls".to_owned());
    }
    let port_raw = draft.smtp_port.trim();
    let port: i32 = port_raw
        .parse()
        .map_err(|_| format!("SMTP 端口必须是 1–65535 的数字，当前为「{port_raw}」"))?;
    if !(1..=65535).contains(&port) {
        return Err(format!("SMTP 端口必须是 1–65535，当前为 {port}"));
    }
    let mail_url = draft.mail_url.trim();
    let mail_from = draft.mail_from.trim();
    let smtp_host = draft.smtp_host.trim();
    let smtp_from = draft.smtp_from.trim();
    if transport == "http" {
        if !(mail_url.starts_with("https://") || mail_url.starts_with("http://")) {
            return Err("HTTP 邮件 URL 必须以 http:// 或 https:// 开头".to_owned());
        }
        if mail_from.is_empty() {
            return Err("HTTP 邮件发件人不能为空".to_owned());
        }
    } else {
        if smtp_host.is_empty() {
            return Err("SMTP 服务器不能为空".to_owned());
        }
        if smtp_from.is_empty() {
            return Err("SMTP 发件人不能为空".to_owned());
        }
    }
    Ok(PortalSettingsBody {
        mail_transport: transport.to_owned(),
        mail_url: mail_url.to_owned(),
        mail_token: secret_wire_value(&draft.mail_token),
        mail_from: mail_from.to_owned(),
        mail_origin: draft.mail_origin.trim().to_owned(),
        smtp_host: smtp_host.to_owned(),
        smtp_port: port,
        smtp_username: draft.smtp_username.trim().to_owned(),
        smtp_password: secret_wire_value(&draft.smtp_password),
        smtp_tls: smtp_tls.to_owned(),
        smtp_from: smtp_from.to_owned(),
    })
}

/// Build the provider body, validating the name and the redirect URI.
pub(crate) fn provider_body(draft: &ProviderDraft) -> Result<AuthProviderBody, String> {
    let provider = draft.provider.trim();
    let bytes = provider.as_bytes();
    if bytes.is_empty()
        || bytes.len() > 64
        || !bytes[0].is_ascii_lowercase()
        || !bytes[1..]
            .iter()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || *byte == b'-')
    {
        return Err("provider 名称只能用小写字母、数字和连字符（首字母为字母）".to_owned());
    }
    let client_id = draft.client_id.trim();
    if client_id.is_empty() {
        return Err("client_id 不能为空".to_owned());
    }
    let redirect_uri = draft.redirect_uri.trim();
    if !(redirect_uri.starts_with("https://")
        || redirect_uri.starts_with("http://localhost")
        || redirect_uri.starts_with("http://127.0.0.1"))
    {
        return Err("回调地址必须是 https://，本地测试可用 http://localhost".to_owned());
    }
    Ok(AuthProviderBody {
        client_id: client_id.to_owned(),
        client_secret: secret_wire_value(&draft.client_secret),
        redirect_uri: redirect_uri.to_owned(),
        enabled: draft.enabled,
        extra: draft.extra.clone(),
    })
}

/// Map one save/delete outcome to the toast text and whether it is a success.
/// Only `Ok` from the Gateway counts as success; callers pass validation and
/// transport failures in as `Err` so both paths share this mapping.
#[must_use]
pub(crate) fn notice(action: &str, outcome: Result<(), String>) -> (bool, String) {
    match outcome {
        Ok(()) => (
            true,
            format!("{action}成功：已写入 Gateway 数据库并立即生效"),
        ),
        Err(error) => (false, format!("{action}失败：{error}")),
    }
}

/// Failure body of a non-2xx admin answer: the handler's `error` field when
/// present, otherwise a bounded slice of the raw body.
fn failure_message(body: &str) -> String {
    if let Ok(value) = serde_json::from_str::<serde_json::Value>(body)
        && let Some(message) = value.get("error").and_then(serde_json::Value::as_str)
    {
        return message.to_owned();
    }
    let bounded: String = body.chars().take(512).collect();
    if bounded.is_empty() {
        "Gateway 未返回错误信息".to_owned()
    } else {
        bounded
    }
}

/// Reuse the Gateway's `ClientError` contract so `GatewayRuntime::admin_call`
/// keeps mapping connection failures onto the status bar's unreachable state.
#[derive(Clone)]
pub(crate) struct PortalApi {
    http: reqwest::Client,
    base: String,
    admin_key: Arc<str>,
}

impl PortalApi {
    pub(crate) fn new(listen: &str, admin_key: &str) -> Self {
        let base = if listen.contains("://") {
            listen.trim_end_matches('/').to_owned()
        } else {
            format!("http://{}", listen.trim_end_matches('/'))
        };
        let http = reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());
        Self {
            http,
            base,
            admin_key: Arc::from(admin_key),
        }
    }

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base)
    }

    fn request(&self, method: reqwest::Method, path: &str) -> reqwest::RequestBuilder {
        self.http
            .request(method, self.url(path))
            .bearer_auth(&*self.admin_key)
    }

    pub(crate) async fn settings(&self) -> Result<PortalSettingsView, ClientError> {
        let response = self
            .request(reqwest::Method::GET, SETTINGS_PATH)
            .send()
            .await
            .map_err(ClientError::Transport)?;
        decode(response).await
    }

    pub(crate) async fn put_settings(
        &self,
        body: &PortalSettingsBody,
    ) -> Result<PortalSettingsView, ClientError> {
        let response = self
            .request(reqwest::Method::PUT, SETTINGS_PATH)
            .json(body)
            .send()
            .await
            .map_err(ClientError::Transport)?;
        decode(response).await
    }

    pub(crate) async fn providers(&self) -> Result<Vec<AuthProviderView>, ClientError> {
        let response = self
            .request(reqwest::Method::GET, PROVIDERS_PATH)
            .send()
            .await
            .map_err(ClientError::Transport)?;
        decode(response).await
    }

    pub(crate) async fn put_provider(
        &self,
        provider: &str,
        body: &AuthProviderBody,
    ) -> Result<AuthProviderView, ClientError> {
        let response = self
            .request(
                reqwest::Method::PUT,
                &format!("{PROVIDERS_PATH}/{provider}"),
            )
            .json(body)
            .send()
            .await
            .map_err(ClientError::Transport)?;
        decode(response).await
    }

    pub(crate) async fn delete_provider(&self, provider: &str) -> Result<(), ClientError> {
        let response = self
            .request(
                reqwest::Method::DELETE,
                &format!("{PROVIDERS_PATH}/{provider}"),
            )
            .send()
            .await
            .map_err(ClientError::Transport)?;
        ensure_ok(response).await
    }
}

async fn decode<T: serde::de::DeserializeOwned>(
    response: reqwest::Response,
) -> Result<T, ClientError> {
    let status = response.status();
    let text = response.text().await.map_err(ClientError::Transport)?;
    if !status.is_success() {
        return Err(ClientError::Http {
            status,
            body: failure_message(&text),
        });
    }
    serde_json::from_str(&text).map_err(|error| ClientError::Decode(error.to_string()))
}

async fn ensure_ok(response: reqwest::Response) -> Result<(), ClientError> {
    let status = response.status();
    if status.is_success() {
        return Ok(());
    }
    let text = response.text().await.map_err(ClientError::Transport)?;
    Err(ClientError::Http {
        status,
        body: failure_message(&text),
    })
}
