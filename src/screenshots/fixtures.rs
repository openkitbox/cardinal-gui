//! Loads the fixed fixture JSON files, validates them against the real gateway
//! DTOs, and turns them into the mock server's response map. Validation here is
//! deliberate: a DTO field rename makes the tool fail fast with a serde error
//! instead of silently rendering empty pages.

use std::{collections::HashMap, path::PathBuf};

use gateway::{
    activity::ActivityEvent,
    admin_ops::{Bootstrap, KeyQuotaRow},
    store::{GroupRow, RequestRow, TrendRow, UsageWindow, UserRow},
};
use jiff::{SignedDuration, Timestamp};

use super::mock_server::MockRoutes;

/// The (provider, key) pair the edit-form scene opens. Derived from the
/// fixture, so the reveal mock and the UI action cannot drift apart.
#[derive(Clone, Debug)]
pub(crate) struct EditTarget {
    pub provider_id: i64,
    pub key_id: i64,
    pub reveal_path: String,
}

pub(crate) struct Fixture {
    pub routes: MockRoutes,
    pub edit_target: EditTarget,
}

#[derive(serde::Deserialize)]
struct Extras {
    users: Vec<UserRow>,
    groups: Vec<GroupRow>,
    quotas: Vec<KeyQuotaRow>,
    usage_window: UsageWindow,
    usage_trend: Vec<TrendRow>,
}

pub(crate) fn load() -> Result<Fixture, String> {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("fixtures/screenshots");
    let bootstrap_file = read(&dir.join("bootstrap.json"))?;
    let extras_text = read(&dir.join("extras.json"))?;
    let mut raw: serde_json::Value = serde_json::from_str(&bootstrap_file)
        .map_err(|e| format!("fixtures/bootstrap.json is not JSON: {e}"))?;
    let now = Timestamp::now();

    // The file keeps stable placeholder timestamps; a screenshot taken days
    // later still needs a "recent success" in the smart group summary. Patch
    // the parsed document once, so the REST body and the SSE payload below
    // carry identical data (apart from the event tag) rather than two
    // independently generated variants.
    let successes = [
        (10_i64, "gpt-4o-mini", 8_i64),
        (10, "gpt-4o", 24),
        (11, "gpt-4o-mini", 40),
        (12, "claude-sonnet-4-5", 15),
    ];
    let success_rows: Vec<serde_json::Value> = successes
        .iter()
        .map(|(key_id, model, ago)| {
            serde_json::json!({
                "key_id": key_id,
                "served_model": model,
                "last_ok": now - SignedDuration::from_secs(*ago),
            })
        })
        .collect();
    let success_array = raw
        .get_mut("model_success")
        .and_then(serde_json::Value::as_array_mut)
        .ok_or("fixtures/bootstrap.json has no model_success array")?;
    *success_array = success_rows;
    let bootstrap_text = serde_json::to_string(&raw)
        .map_err(|e| format!("serialize fixtures/bootstrap.json: {e}"))?;

    // Validate against the gateway's own DTOs — this is the whole point of
    // using typed fixtures rather than opaque JSON blobs.
    let bootstrap: Bootstrap = serde_json::from_str(&bootstrap_text)
        .map_err(|e| format!("fixtures/bootstrap.json does not decode as Bootstrap: {e}"))?;
    let extras: Extras = serde_json::from_str(&extras_text)
        .map_err(|e| format!("fixtures/extras.json does not decode: {e}"))?;

    // Derive the edit scene's target from the same Bootstrap the GUI reads.
    // A static key is required because only pasted credentials are revealed.
    let edit_key = bootstrap
        .keys
        .iter()
        .find(|key| key.auth_kind == "static")
        .ok_or("fixtures/bootstrap.json has no static key for the edit scene")?;
    let edit_target = EditTarget {
        provider_id: edit_key.provider_id,
        key_id: edit_key.id,
        reveal_path: format!(
            "/api/providers/{}/keys/{}/reveal",
            edit_key.provider_id, edit_key.id
        ),
    };
    let edit_label = edit_key.label.clone();

    // The SSE envelope adds only the internally tagged variant's `t` field;
    // the bootstrap payload is the same document served by `/api/bootstrap`.
    let mut event = raw.clone();
    event
        .as_object_mut()
        .ok_or("fixtures/bootstrap.json is not an object")?
        .insert("t".to_owned(), serde_json::json!("bootstrap"));
    let bootstrap_event =
        serde_json::to_string(&event).map_err(|e| format!("serialize bootstrap SSE event: {e}"))?;
    serde_json::from_str::<ActivityEvent>(&bootstrap_event)
        .map_err(|e| format!("fixtures bootstrap SSE event does not decode: {e}"))?;

    // One in-flight attempt for the first `k2-256k` rotate entry: it stays
    // live (no End) so the screenshot exercises the 「当前」 marker. The
    // caller/agent pair is one already in the recent-ledger client set, so
    // the feed does not introduce a fifth client card.
    let live_event = serde_json::to_string(&serde_json::json!({
        "t": "start",
        "origin": 1,
        "seq": 1,
        "client_model": "k2-256k",
        "route": "k2-256k",
        "account": "主用",
        "served_model": "gpt-4o",
        "protocol": "openai_chat",
        "client_protocol": "openai_chat",
        "effort": "medium",
        "caller": "cardinal-antb",
        "caller_key_id": null,
        "caller_user_id": null,
        "agent": "antb",
        "key_id": 10,
        "stream": true,
    }))
    .map_err(|e| format!("serialize live SSE event: {e}"))?;
    serde_json::from_str::<ActivityEvent>(&live_event)
        .map_err(|e| format!("fixtures live SSE event does not decode: {e}"))?;

    let array = |key: &str| -> Result<serde_json::Value, String> {
        raw.get(key)
            .cloned()
            .ok_or_else(|| format!("fixtures/bootstrap.json missing {key}"))
    };
    fn items<T: serde::Serialize>(value: T) -> serde_json::Value {
        serde_json::json!({ "items": value })
    }

    let mut bodies = HashMap::new();
    bodies.insert("/health".to_owned(), "ok".to_owned());
    bodies.insert("/api/bootstrap".to_owned(), bootstrap_text.clone());
    // Portal settings are part of the settings-page visual fixtures as well as
    // the route/provider scenes. Keep the values representative but fake: the
    // screenshot server never serves real credentials.
    bodies.insert(
        "/api/portal/settings".to_owned(),
        serde_json::json!({
            "mail_transport": "http",
            "mail_url": "https://mail.example.test/send",
            "mail_token": "********",
            "mail_from": "Cardinal <noreply@example.test>",
            "mail_origin": "https://portal.example.test",
            "smtp_host": "smtp.example.test",
            "smtp_port": 587,
            "smtp_username": "cardinal",
            "smtp_password": "********",
            "smtp_tls": "starttls",
            "smtp_from": "Cardinal <noreply@example.test>"
        })
        .to_string(),
    );
    bodies.insert(
        "/api/portal/providers".to_owned(),
        serde_json::json!([
            {
                "provider": "google",
                "client_id": "google-client-fixture",
                "client_secret": "********",
                "redirect_uri": "https://portal.example.test/auth/google/callback",
                "enabled": true,
                "extra": {}
            },
            {
                "provider": "github",
                "client_id": "github-client-fixture",
                "client_secret": "********",
                "redirect_uri": "https://portal.example.test/auth/github/callback",
                "enabled": false,
                "extra": {}
            }
        ])
        .to_string(),
    );
    bodies.insert(
        "/api/providers".to_owned(),
        serde_json::to_string(&raw.get("providers")).map_err(|e| e.to_string())?,
    );
    bodies.insert(
        "/api/routes".to_owned(),
        serde_json::to_string(&raw.get("routes")).map_err(|e| e.to_string())?,
    );
    bodies.insert(
        "/api/rests".to_owned(),
        serde_json::to_string(&items(array("rests")?)).map_err(|e| e.to_string())?,
    );
    bodies.insert(
        "/api/quota-snapshots".to_owned(),
        serde_json::to_string(&items(array("quota_snapshots")?)).map_err(|e| e.to_string())?,
    );
    bodies.insert(
        "/api/requests/model-success".to_owned(),
        serde_json::to_string(&items(array("model_success")?)).map_err(|e| e.to_string())?,
    );
    bodies.insert(
        "/api/requests".to_owned(),
        serde_json::to_string(&items(request_rows(now))).map_err(|e| e.to_string())?,
    );
    bodies.insert(
        "/api/chat-models".to_owned(),
        serde_json::json!({ "models": ["gpt-4o-mini", "claude-sonnet-4-5"] }).to_string(),
    );
    bodies.insert(
        "/api/gateway-keys".to_owned(),
        serde_json::to_string(&items(array("gateway_keys")?)).map_err(|e| e.to_string())?,
    );
    bodies.insert(
        "/api/users".to_owned(),
        serde_json::to_string(&items(extras.users)).map_err(|e| e.to_string())?,
    );
    bodies.insert(
        "/api/user-groups".to_owned(),
        serde_json::to_string(&items(extras.groups)).map_err(|e| e.to_string())?,
    );
    bodies.insert(
        "/api/quotas".to_owned(),
        serde_json::to_string(&items(extras.quotas)).map_err(|e| e.to_string())?,
    );
    bodies.insert(
        "/api/usage/window".to_owned(),
        serde_json::to_string(&extras.usage_window).map_err(|e| e.to_string())?,
    );
    bodies.insert(
        "/api/usage/trend".to_owned(),
        serde_json::to_string(&items(extras.usage_trend)).map_err(|e| e.to_string())?,
    );
    bodies.insert(
        "/api/concurrency".to_owned(),
        serde_json::to_string(&items(serde_json::json!([]))).map_err(|e| e.to_string())?,
    );
    // The key editor reveals one plaintext credential; serve an obvious
    // placeholder (fixtures carry no real secret).
    bodies.insert(
        edit_target.reveal_path.clone(),
        serde_json::json!({
            "id": edit_target.key_id,
            "provider_id": edit_target.provider_id,
            "label": edit_label,
            "api_key": "mock-revealed-key-placeholder",
            "enabled": true,
            "position": 0,
            "max_concurrency": 8,
            "inflight": 2,
            "models": [],
            "allowed_models": null,
            "auth_kind": "static",
            "archived_at": null,
            "proxy_configured": true
        })
        .to_string(),
    );

    Ok(Fixture {
        routes: MockRoutes {
            bodies,
            bootstrap_event,
            live_event: Some(live_event),
        },
        edit_target,
    })
}

fn read(path: &PathBuf) -> Result<String, String> {
    std::fs::read_to_string(path)
        .map_err(|e| format!("read {}: {e} (run from the worktree root)", path.display()))
}

/// Recent ledger rows for the routes scene: four client identities, both
/// serving accounts used by the route entries, and non-null latencies so the
/// smart rows show a measured time. Times are relative to capture time — a
/// fixed timestamp would fall out of the 60-second rate window.
fn request_rows(now: Timestamp) -> Vec<RequestRow> {
    const CLIENTS: [(&str, &str); 4] = [
        ("cardinal-antb", "antb"),
        ("cardinal-claude-code", "claude-code"),
        ("cardinal-openclaw", "openclaw"),
        ("cardinal-cherry-studio", "cherry-studio"),
    ];

    (0_i32..30)
        .map(|index| {
            // Newest first: reverse the client cycle so the oldest (seeded
            // first) client is antb, matching the reference's selected card.
            let (caller, agent) = CLIENTS[(29 - index) as usize % CLIENTS.len()];
            let (provider_id, key_id, model, key_label, alias) = match index % 4 {
                0 | 1 => (1, 10, "gpt-4o-mini", "主用", "deepseek"),
                2 => (2, 12, "claude-sonnet-4-5", "Claude Pro", "k3-256k"),
                _ => (1, 10, "gpt-4o", "主用", "ertong-fabel"),
            };
            RequestRow {
                id: 9_001 + i64::from(index),
                ts: now - SignedDuration::from_secs(i64::from(2 + index)),
                protocol: "openai_chat".to_owned(),
                client_protocol: Some("openai_chat".to_owned()),
                effort: Some("medium".to_owned()),
                alias: Some(alias.to_owned()),
                provider_id: Some(provider_id),
                provider_key_id: Some(key_id),
                model: Some(model.to_owned()),
                served_model: Some(model.to_owned()),
                status: 200,
                input_tokens: Some(1_200 + i64::from(index) * 37),
                output_tokens: Some(180 + i64::from(index) * 11),
                cached_input_tokens: Some(0),
                cache_creation_input_tokens: None,
                total_ms: Some(240 + (index * 23) % 620),
                ttft_ms: Some(90 + i64::from(index * 7 % 200)),
                cost_usd: Some(0.0012 + f64::from(index) * 0.0003),
                error: None,
                caller_key: Some(caller.to_owned()),
                agent: Some(agent.to_owned()),
                request_id: Some(format!("req_fixture_{}", 9_001 + index)),
                key_label: Some(key_label.to_owned()),
                user_id: Some(1),
                login: Some("owner".to_owned()),
                gateway_key_id: Some(100),
                charged_microus: Some(1_200 + i64::from(index) * 13),
                rate_bp: Some(10_000),
            }
        })
        .collect()
}
