//! The hardcoded provider catalog: endpoints/protocols almost never change,
//! so they are fixed data here instead of editable UI. kimi/glm additionally
//! offer 区域（国内/国际）× Coding Plan variants ([`resolve`]): the choice is
//! folded into the stored provider `name` (kimi-code-intl 等), so variants
//! coexist as independent provider rows.

use crate::assets::provider_logos;

/// How a provider's icon is rendered on cards and the picker.
pub(super) enum ProviderIcon {
    /// SVG logo embedded in the binary (width/height patched to 96px at import
    /// so resvg rasterizes crisply), rendered full-color via `img()`.
    Svg(&'static [u8]),
}

/// One entry of the hardcoded provider catalog. `name` is the value stored in
/// `gateway_providers.name`.
pub(super) struct ProviderSpec {
    pub(super) name: &'static str,
    pub(super) display: &'static str,
    pub(super) base_url: &'static str,
    pub(super) protocol: &'static str,
    pub(super) auth_scheme: &'static str,
    pub(super) icon: ProviderIcon,
}

/// The provider catalog. Adding a key picks from this list; the provider row
/// is created on demand with exactly these parameters.
pub(super) const PROVIDER_SPECS: &[ProviderSpec] = &[
    ProviderSpec {
        name: "gemini",
        display: "Gemini",
        // Google's native Gemini API: the base is the version root; the relay
        // bakes models/{model}:generateContent into the URL, the probe lists at
        // {base}/models with x-goog-api-key.
        base_url: "https://generativelanguage.googleapis.com/v1beta",
        protocol: "gemini",
        auth_scheme: "x_goog_api_key",
        icon: ProviderIcon::Svg(provider_logos::GEMINI),
    },
    ProviderSpec {
        name: "glm",
        display: "GLM 智谱",
        base_url: "https://open.bigmodel.cn/api/paas/v4",
        protocol: "openai_chat",
        auth_scheme: "bearer",
        icon: ProviderIcon::Svg(provider_logos::GLM),
    },
    ProviderSpec {
        name: "anthropic",
        display: "Anthropic",
        base_url: "https://api.anthropic.com",
        protocol: "anthropic_messages",
        auth_scheme: "x_api_key",
        icon: ProviderIcon::Svg(provider_logos::ANTHROPIC),
    },
    ProviderSpec {
        name: "openai",
        display: "OpenAI",
        base_url: "https://api.openai.com/v1",
        protocol: "openai_chat",
        auth_scheme: "bearer",
        icon: ProviderIcon::Svg(provider_logos::OPENAI),
    },
    ProviderSpec {
        name: "deepseek",
        display: "DeepSeek",
        base_url: "https://api.deepseek.com",
        protocol: "openai_chat",
        auth_scheme: "bearer",
        icon: ProviderIcon::Svg(provider_logos::DEEPSEEK),
    },
    ProviderSpec {
        name: "kimi",
        display: "Kimi 月之暗面",
        // Host root, not a full endpoint: the capability table appends
        // /v1/chat/completions, /anthropic/v1/messages, /v1/responses.
        base_url: "https://api.moonshot.cn",
        protocol: "openai_chat",
        auth_scheme: "bearer",
        icon: ProviderIcon::Svg(provider_logos::KIMI),
    },
    ProviderSpec {
        name: "opencode-go",
        display: "OpenCode Go",
        // OpenCode's subscription relay for open coding models: the gateway
        // root — the capability table
        // appends /v1/chat/completions, /v1/messages, /v1/responses, and the
        // relay adds OpenCode's mandatory x-opencode-session header family for
        // opencode.ai upstreams itself.
        base_url: "https://opencode.ai/zen/go",
        protocol: "openai_chat",
        auth_scheme: "bearer",
        icon: ProviderIcon::Svg(provider_logos::OPENCODE),
    },
    ProviderSpec {
        name: "commandcode",
        display: "Command Code",
        // Command Code's Provider API: the /provider root — the capability
        // table appends
        // /v1/chat/completions, /v1/messages, /v1/responses. Its model list is
        // per-model about which endpoint a model answers on (Claude models
        // /messages alone, the rest chat/responses), like opencode.
        base_url: "https://api.commandcode.ai/provider",
        protocol: "openai_chat",
        auth_scheme: "bearer",
        icon: ProviderIcon::Svg(provider_logos::COMMANDCODE),
    },
    // ---- Retrieval (embeddings) providers ----
    // A retrieval row is passthrough-only: the client calls /v1/embeddings and
    // the capability table appends `embeddings` to the version root below.
    ProviderSpec {
        name: "openai-embed",
        display: "OpenAI 向量",
        // A separate row from chat's: a provider row carries ONE protocol, and
        // the relay selects the capability by the client's protocol.
        base_url: "https://api.openai.com/v1",
        protocol: "openai_embeddings",
        auth_scheme: "bearer",
        icon: ProviderIcon::Svg(provider_logos::OPENAI),
    },
    ProviderSpec {
        name: "siliconflow",
        display: "SiliconFlow 硅基流动",
        base_url: "https://api.siliconflow.cn/v1",
        protocol: "openai_embeddings",
        auth_scheme: "bearer",
        icon: ProviderIcon::Svg(provider_logos::OPENAI),
    },
    ProviderSpec {
        name: "jina",
        display: "Jina AI",
        base_url: "https://api.jina.ai/v1",
        protocol: "openai_embeddings",
        auth_scheme: "bearer",
        icon: ProviderIcon::Svg(provider_logos::OPENAI),
    },
    ProviderSpec {
        name: "voyage",
        display: "Voyage AI",
        base_url: "https://api.voyageai.com/v1",
        protocol: "openai_embeddings",
        auth_scheme: "bearer",
        icon: ProviderIcon::Svg(provider_logos::OPENAI),
    },
    ProviderSpec {
        name: "kiro",
        display: "Kiro",
        // Kiro's runtime host is derived from the selected account's
        // api_region; the import form deliberately does not expose an endpoint
        // field and the write flow supplies the derived URL.
        base_url: "",
        protocol: "kiro",
        auth_scheme: "bearer",
        icon: ProviderIcon::Svg(provider_logos::KIRO),
    },
];

/// Look up the catalog entry for a stored provider name: an exact match
/// first ("opencode-go" is a full entry, not vendor + variant), then the
/// first '-'-separated segment — variants (kimi-code-intl, glm-code, …)
/// resolve to their vendor's entry (icon).
pub(super) fn spec_for(provider_name: &str) -> Option<&'static ProviderSpec> {
    PROVIDER_SPECS
        .iter()
        .find(|spec| spec.name == provider_name)
        .or_else(|| {
            let vendor = provider_name.split('-').next().unwrap_or(provider_name);
            PROVIDER_SPECS.iter().find(|spec| spec.name == vendor)
        })
}

/// Brand logo fallback for provider rows whose catalog entry is not available
/// (for example, rows created by an older gateway).
pub(super) fn imported_logo(provider_name: &str) -> Option<&'static [u8]> {
    match provider_name.split('-').next().unwrap_or(provider_name) {
        "kiro" => Some(provider_logos::KIRO),
        _ => None,
    }
}

/// Kiro runtime endpoint for one validated account region.
pub(super) fn kiro_endpoint(region: &str) -> String {
    format!("https://runtime.{region}.kiro.dev")
}

/// Provider region: picks the base_url host for vendors with regional
/// endpoints (kimi/glm). Vendors without a variant table ignore it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum Region {
    Domestic,
    International,
}

impl Region {
    pub(super) const ALL: [Region; 2] = [Region::Domestic, Region::International];

    pub(super) fn label(self) -> &'static str {
        match self {
            Region::Domestic => "国内",
            Region::International => "国际",
        }
    }
}

/// (name, base_url) for one vendor's region × Coding Plan combination;
/// indexed `[coding_plan][region]`.
type VariantTable = [[(&'static str, &'static str); 2]; 2];

/// kimi：月之暗面平台（host 根，能力路径由网关拼接）与 Kimi Code（base 即版本根）。
const KIMI_VARIANTS: VariantTable = [
    [
        ("kimi", "https://api.moonshot.cn"),
        ("kimi-intl", "https://api.moonshot.ai"),
    ],
    [
        ("kimi-code", "https://api.kimi.com/coding/v1"),
        ("kimi-code-intl", "https://api.kimi.ai/coding/v1"),
    ],
];

/// glm：标准版与 Coding Plan（`/api/coding/paas/v4`），国内/国际各一套。
const GLM_VARIANTS: VariantTable = [
    [
        ("glm", "https://open.bigmodel.cn/api/paas/v4"),
        ("glm-intl", "https://api.z.ai/api/paas/v4"),
    ],
    [
        ("glm-code", "https://open.bigmodel.cn/api/coding/paas/v4"),
        ("glm-code-intl", "https://api.z.ai/api/coding/paas/v4"),
    ],
];

/// The region × Coding Plan variant table of a catalog spec, when the vendor
/// has one (only kimi/glm do).
pub(super) fn variants_of(spec: &ProviderSpec) -> Option<&'static VariantTable> {
    match spec.name {
        "kimi" => Some(&KIMI_VARIANTS),
        "glm" => Some(&GLM_VARIANTS),
        _ => None,
    }
}

/// Inverse of [`resolve`]: parse a stored provider row (name folded by
/// `provider_kind`: "glm-code-intl" → vendor glm) back into its (region,
/// coding_plan) — for pre-filling the variant chips when editing an existing
/// provider's endpoint. `None` when the vendor has no variant table (or the
/// name/base_url pair is not a catalog variant); callers then simply hide the
/// chips.
pub(super) fn variant_of_row(name: &str, base_url: &str) -> Option<(Region, bool)> {
    let vendor = name.split('-').next().unwrap_or(name);
    let table = match vendor {
        "kimi" => &KIMI_VARIANTS,
        "glm" => &GLM_VARIANTS,
        _ => return None,
    };
    let base = base_url.trim_end_matches('/');
    for (coding_index, row) in table.iter().enumerate() {
        for (region_index, (variant_name, variant_url)) in row.iter().enumerate() {
            if *variant_name == name && *variant_url == base {
                return Some((Region::ALL[region_index], coding_index == 1));
            }
        }
    }
    None
}

/// Resolve the catalog spec + 区域/Coding Plan choice into the stored
/// (name, base_url). Vendors without a variant table keep the catalog entry
/// (region/plan are not rendered for them).
pub(super) fn resolve(
    spec: &'static ProviderSpec,
    region: Region,
    coding_plan: bool,
) -> (&'static str, &'static str) {
    match variants_of(spec) {
        Some(table) => table[coding_plan as usize][region as usize],
        None => (spec.name, spec.base_url),
    }
}
