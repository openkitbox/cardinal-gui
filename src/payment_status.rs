//! Read-only view of the local Xiaodou payment configuration for the settings
//! page. The GUI has no Gateway admin endpoint that exposes this status, so the
//! card is derived from the same local TOML the Gateway process loads. Only
//! public fields are modelled; no key material is ever stored or rendered.

use std::collections::BTreeMap;

use serde::Deserialize;

/// How much of the local config could be checked. The Gateway's authoritative
/// validity also depends on its environment-injected private key, which is
/// deliberately invisible to the GUI.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PaymentConfigState {
    Configured,
    Incomplete,
    Missing,
    Unavailable,
}

impl PaymentConfigState {
    fn status_text(self) -> &'static str {
        match self {
            Self::Configured => {
                "已配置（本地公开字段校验通过；私钥由 Gateway 环境变量注入，GUI 不可见）"
            }
            Self::Incomplete => "配置不完整（本地公开字段缺失或无效）",
            Self::Missing => "未配置（本地未找到 [gateway.payments.xiaodou]）",
            Self::Unavailable => "无法读取本地配置（状态未知）",
        }
    }
}

/// Public, redacted-by-construction snapshot rendered by the settings card.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PaymentStatusView {
    pub(crate) state: PaymentConfigState,
    pub(crate) channel: String,
    pub(crate) notify_url: String,
    /// `(kid, fingerprint)` pairs; PEM material is never retained.
    pub(crate) platform_keys: Vec<(String, String)>,
}

impl PaymentStatusView {
    pub(crate) fn unavailable() -> Self {
        Self {
            state: PaymentConfigState::Unavailable,
            channel: String::new(),
            notify_url: String::new(),
            platform_keys: Vec::new(),
        }
    }
}

/// Read the open config file. A read failure never claims "not configured":
/// the card says the status is unknown instead.
pub(crate) fn load_local() -> PaymentStatusView {
    let Some(path) = crate::load::config_path() else {
        return PaymentStatusView::unavailable();
    };
    match std::fs::read_to_string(&path) {
        Ok(raw) => from_config_text(&raw),
        Err(error) => {
            tracing::warn!(path = %path.display(), %error, "failed to read config; payment status unavailable");
            PaymentStatusView::unavailable()
        }
    }
}

#[derive(Deserialize)]
struct MainConfig {
    gateway: Option<GatewaySection>,
}

#[derive(Deserialize)]
struct GatewaySection {
    payments: Option<PaymentsSection>,
}

#[derive(Deserialize)]
struct PaymentsSection {
    xiaodou: Option<XiaodouSection>,
}

#[derive(Deserialize)]
struct XiaodouSection {
    channel: Option<String>,
    notify_url: Option<String>,
    #[serde(default)]
    platform_keys: BTreeMap<String, PlatformKeySection>,
}

#[derive(Deserialize)]
struct PlatformKeySection {
    fingerprint: Option<String>,
}

/// Parse only the public Xiaodou fields from the main config text. Unknown keys
/// (including a mistakenly written private key or a platform PEM) are ignored,
/// so they cannot reach the view, its `Debug`, or the rendered card.
pub(crate) fn from_config_text(raw: &str) -> PaymentStatusView {
    let Ok(main) = toml::from_str::<MainConfig>(raw) else {
        return PaymentStatusView::unavailable();
    };
    let Some(xiaodou) = main
        .gateway
        .and_then(|gateway| gateway.payments)
        .and_then(|payments| payments.xiaodou)
    else {
        return PaymentStatusView {
            state: PaymentConfigState::Missing,
            channel: String::new(),
            notify_url: String::new(),
            platform_keys: Vec::new(),
        };
    };

    let channel = xiaodou.channel.unwrap_or_default();
    let notify_url = xiaodou.notify_url.unwrap_or_default();
    let platform_keys: Vec<(String, String)> = xiaodou
        .platform_keys
        .into_iter()
        .filter_map(|(kid, key)| {
            key.fingerprint
                .filter(|fingerprint| !fingerprint.is_empty())
                .map(|fingerprint| (kid, fingerprint))
        })
        .collect();
    let valid = channel == "ALIPAY_H5"
        && notify_url.starts_with("https://")
        && !notify_url.contains(['?', '#'])
        && !platform_keys.is_empty();
    PaymentStatusView {
        state: if valid {
            PaymentConfigState::Configured
        } else {
            PaymentConfigState::Incomplete
        },
        channel,
        notify_url,
        platform_keys,
    }
}

fn shown(value: &str) -> String {
    if value.is_empty() {
        "—".to_owned()
    } else {
        value.to_owned()
    }
}

/// The exact label/value pairs the settings card renders, kept here so tests
/// assert on the strings a user actually sees.
pub(crate) fn card_rows(view: &PaymentStatusView) -> Vec<(&'static str, String)> {
    let keys = view
        .platform_keys
        .iter()
        .map(|(kid, fingerprint)| format!("{kid} → {fingerprint}"))
        .collect::<Vec<_>>()
        .join("；");
    vec![
        ("配置状态", view.state.status_text().to_owned()),
        ("通道", shown(&view.channel)),
        ("回调地址", shown(&view.notify_url)),
        ("平台公钥", shown(&keys)),
    ]
}

/// All card text concatenated, for leak assertions.
#[cfg(test)]
pub(crate) fn card_text(view: &PaymentStatusView) -> String {
    card_rows(view)
        .into_iter()
        .map(|(label, value)| format!("{label} {value}"))
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    const CONFIGURED: &str = r#"
[gateway]
listen = "127.0.0.1:8787"

[gateway.payments.xiaodou]
base_url = "https://api.xiaodou.example.com"
app_id = "xdop_1234567890abcdef"
merchant_kid = "merchant-2026"
channel = "ALIPAY_H5"
notify_url = "https://c1.iios.net/payments/webhook/xiaodou"
private_key = "-----BEGIN RSA PRIVATE KEY-----\nTOP-SECRET-PRIVATE-KEY\n-----END RSA PRIVATE KEY-----"

[gateway.payments.xiaodou.platform_keys.k2026]
pem = "-----BEGIN PUBLIC KEY-----\nSECRET-PLATFORM-PEM\n-----END PUBLIC KEY-----"
fingerprint = "AA:BB:CC:DD"
"#;

    #[test]
    fn configured_card_shows_public_fields_and_hides_key_material() {
        let view = from_config_text(CONFIGURED);
        assert_eq!(view.state, PaymentConfigState::Configured);

        let rows = card_rows(&view);
        assert!(
            rows.iter()
                .any(|(label, value)| *label == "通道" && value == "ALIPAY_H5")
        );
        assert!(rows.iter().any(|(label, value)| *label == "回调地址"
            && value == "https://c1.iios.net/payments/webhook/xiaodou"));
        assert!(
            rows.iter()
                .any(|(label, value)| *label == "平台公钥" && value == "k2026 → AA:BB:CC:DD")
        );
        assert!(
            rows.iter()
                .any(|(label, value)| *label == "配置状态" && value.contains("本地公开字段"))
        );

        let rendered = card_text(&view);
        assert!(!rendered.contains("TOP-SECRET-PRIVATE-KEY"));
        assert!(!rendered.contains("SECRET-PLATFORM-PEM"));
        assert!(!rendered.contains("PRIVATE KEY"));
        assert!(!format!("{view:?}").contains("TOP-SECRET-PRIVATE-KEY"));
    }

    #[test]
    fn missing_and_incomplete_config_have_distinct_clear_states() {
        let missing = from_config_text("[gateway]\nlisten = \"127.0.0.1:8787\"\n");
        assert_eq!(missing.state, PaymentConfigState::Missing);
        assert!(card_text(&missing).contains("未配置"));

        let wrong_channel = CONFIGURED.replace("ALIPAY_H5", "WECHAT_H5");
        let incomplete = from_config_text(&wrong_channel);
        assert_eq!(incomplete.state, PaymentConfigState::Incomplete);
        assert!(card_text(&incomplete).contains("配置不完整"));

        let no_keys = r#"
[gateway.payments.xiaodou]
channel = "ALIPAY_H5"
notify_url = "https://c1.iios.net/payments/webhook/xiaodou"
"#;
        assert_eq!(
            from_config_text(no_keys).state,
            PaymentConfigState::Incomplete
        );
    }

    #[test]
    fn unavailable_config_is_explicit_instead_of_claiming_missing() {
        let view = PaymentStatusView::unavailable();
        assert_eq!(view.state, PaymentConfigState::Unavailable);
        let rendered = card_text(&view);
        assert!(rendered.contains("无法读取本地配置"));
        assert!(!rendered.contains("未配置"));
    }
}
