//! Loads the fixed fixture JSON files, validates them against the real gateway
//! DTOs, and turns them into the mock server's response map. Validation here is
//! deliberate: a DTO field rename makes the tool fail fast with a serde error
//! instead of silently rendering empty pages.

use std::{collections::HashMap, path::PathBuf, sync::Arc};

use gateway::{
    activity::ActivityEvent,
    admin_ops::{Bootstrap, KeyQuotaRow},
    store::{GroupRow, TrendRow, UsageWindow, UserRow},
};

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
    let bootstrap_text = read(&dir.join("bootstrap.json"))?;
    let extras_text = read(&dir.join("extras.json"))?;

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

    let bootstrap_event = serde_json::to_string(&ActivityEvent::Bootstrap(Arc::new(bootstrap)))
        .map_err(|e| format!("serialize bootstrap SSE event: {e}"))?;

    let raw: serde_json::Value = serde_json::from_str(&bootstrap_text)
        .map_err(|e| format!("fixtures/bootstrap.json is not JSON: {e}"))?;
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
        serde_json::to_string(&items(serde_json::json!([]))).map_err(|e| e.to_string())?,
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
        },
        edit_target,
    })
}

fn read(path: &PathBuf) -> Result<String, String> {
    std::fs::read_to_string(path)
        .map_err(|e| format!("read {}: {e} (run from the worktree root)", path.display()))
}
