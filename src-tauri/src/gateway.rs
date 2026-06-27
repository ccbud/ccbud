// Gateway core — Rust port of proxy.js. Phase 2.
//
// Implemented so far: the deterministic ROUTING logic (resolve_routing + helpers), with an
// in-binary selftest (`routing_selftest`) mirroring the 8 routing cases in test/selftest.js.
// Still to come: SSE transform (model rewrite + usage sniff), upstream forwarding via reqwest
// (gzip/deflate/br decode, 429 retry), /v1/models merge/synthesize, and the localhost server.

// Phase 2 in progress: a few pub items (CLAUDE_TIER_MODELS …) are consumed once /v1/models
// synthesis and forwarding land; silence dead_code until then.
#![allow(dead_code)]

use serde_json::{json, Value};
use std::collections::HashSet;

/// Standard Claude tier names ccbud advertises to clients (claudeModels.js).
pub const CLAUDE_TIER_MODELS: &[(&str, &str)] = &[
    ("claude-opus-4-6", "opus"),
    ("claude-sonnet-4-6", "sonnet"),
    ("claude-haiku-4-5", "haiku"),
];

/// Heuristic: is this model name a "small / fast" tier?
fn looks_small(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    [
        "haiku", "small", "fast", "mini", "air", "flash", "lite", "nano", "tiny", "turbo",
    ]
    .iter()
    .any(|k| n.contains(k))
}

/// Is this one of Claude's own default model names (the only names we auto-remap)?
fn is_claude_default(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    n.starts_with("claude-") || n.starts_with("claude_")
}

#[derive(Debug, Clone)]
pub struct Routing {
    pub provider_id: String,
    pub outgoing_model: Option<String>,
    pub client_facing_model: Option<String>,
}

/// Decide how to route a request and translate its model name. Mirrors proxy.js `resolveRouting`.
/// Every request goes to the single ACTIVE provider; the requested model id is resolved in order:
///   1. an active-provider Custom alias        -> map alias -> the user's upstream name
///   2. the provider's PRIMARY / LIGHTWEIGHT    -> passthrough
///   3. a model the provider really has         -> passthrough (configured upstream, or in known_models)
///   4. otherwise (default mapping on): claude main tiers -> PRIMARY, everything else -> LIGHTWEIGHT;
///      with mapDefaultModels:false -> forwarded untouched.
pub fn resolve_routing(
    requested_model: Option<&str>,
    config: &Value,
    known_models: Option<&HashSet<String>>,
) -> Option<Routing> {
    let providers = config.get("providers")?.as_array()?;
    if providers.is_empty() {
        return None;
    }
    let active_id = config.get("activeProviderId").and_then(|v| v.as_str());
    let active = providers
        .iter()
        .find(|p| p.get("id").and_then(|v| v.as_str()) == active_id)
        .or_else(|| providers.first())?;
    let pid = active
        .get("id")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    let pass = |m: &str| {
        Some(Routing {
            provider_id: pid.clone(),
            outgoing_model: Some(m.to_string()),
            client_facing_model: Some(m.to_string()),
        })
    };

    let requested = match requested_model {
        None => {
            return Some(Routing {
                provider_id: pid.clone(),
                outgoing_model: None,
                client_facing_model: None,
            })
        }
        Some(m) => m,
    };

    let primary = active.get("defaultModel").and_then(|v| v.as_str()).unwrap_or("");
    let light = active.get("smallFastModel").and_then(|v| v.as_str()).unwrap_or("");
    let models = active.get("models").and_then(|v| v.as_array());

    // 1) active-provider custom alias -> upstream
    if let Some(ms) = models {
        for m in ms {
            let alias = m.get("alias").and_then(|v| v.as_str()).unwrap_or("");
            let upstream = m.get("upstream").and_then(|v| v.as_str()).unwrap_or("");
            if !alias.is_empty() && alias == requested && !upstream.is_empty() {
                return Some(Routing {
                    provider_id: pid.clone(),
                    outgoing_model: Some(upstream.to_string()),
                    client_facing_model: Some(requested.to_string()),
                });
            }
        }
    }
    // 2) already the provider's primary / lightweight -> passthrough
    if requested == primary || requested == light {
        return pass(requested);
    }
    // 3) a model the provider really has -> passthrough
    if let Some(ms) = models {
        for m in ms {
            if m.get("upstream").and_then(|v| v.as_str()) == Some(requested) {
                return pass(requested);
            }
        }
    }
    if let Some(known) = known_models {
        if known.contains(requested) {
            return pass(requested);
        }
    }
    // 4) unconfigured id. default-mapping off -> forward untouched
    let map_default = active
        .get("mapDefaultModels")
        .map(|v| v.as_bool().unwrap_or(true))
        .unwrap_or(true);
    if !map_default {
        return pass(requested);
    }
    // map onto the active provider: claude main tiers -> PRIMARY, everything else -> LIGHTWEIGHT
    let big = if !primary.is_empty() { primary } else { light };
    let small = if !light.is_empty() { light } else { primary };
    let target = if is_claude_default(requested) {
        if looks_small(requested) { small } else { big }
    } else {
        small
    };
    if !target.is_empty() {
        return Some(Routing {
            provider_id: pid.clone(),
            outgoing_model: Some(target.to_string()),
            client_facing_model: Some(requested.to_string()),
        });
    }
    pass(requested)
}

/// In-binary equivalent of test/selftest.js's 8 routing unit checks. Returns a JSON summary so
/// the self-check channel can confirm Rust routing matches the Electron core exactly.
pub fn routing_selftest() -> Value {
    let config = json!({ "port":0, "activeProviderId":"glm", "providers":[
        { "id":"glm","name":"GLM","baseUrl":"https://x","authToken":"","defaultModel":"glm-5.1","smallFastModel":"glm-5.1","mapDefaultModels":true,"models":[{"alias":"claude-opus-4.8[1m]","upstream":"glm-5.1"}] }
    ]});
    let cfg2 = json!({ "port":0, "activeProviderId":"main", "providers":[
        { "id":"main","name":"Main","baseUrl":"http://127.0.0.1:1","authToken":"k","defaultModel":"big-model","smallFastModel":"small-model","mapDefaultModels":true,"models":[{"alias":"my-alias","upstream":"aliased-up"}] },
        { "id":"other","name":"Other","baseUrl":"http://127.0.0.1:2","authToken":"k","defaultModel":"other-big","smallFastModel":"other-small","mapDefaultModels":true,"models":[{"alias":"other-alias","upstream":"other-up"}] }
    ]});
    let off = json!({ "port":0, "activeProviderId":"m", "providers":[
        { "id":"m","name":"M","baseUrl":"http://127.0.0.1:1","authToken":"k","defaultModel":"big","smallFastModel":"small","mapDefaultModels":false,"models":[] }
    ]});

    let out = |r: &Option<Routing>| r.as_ref().and_then(|x| x.outgoing_model.clone());
    let cf = |r: &Option<Routing>| r.as_ref().and_then(|x| x.client_facing_model.clone());
    let pidf = |r: &Option<Routing>| r.as_ref().map(|x| x.provider_id.clone());

    let mut fails: Vec<String> = vec![];
    let mut n = 0;
    let mut chk = |name: &str, cond: bool| {
        n += 1;
        if !cond {
            fails.push(name.to_string());
        }
    };

    let r = resolve_routing(Some("claude-opus-4.8[1m]"), &config, None);
    chk("1 alias→upstream", out(&r).as_deref() == Some("glm-5.1") && cf(&r).as_deref() == Some("claude-opus-4.8[1m]"));

    let r = resolve_routing(Some("glm-5.1"), &config, None);
    chk("2 real passthrough", out(&r).as_deref() == Some("glm-5.1") && cf(&r).as_deref() == Some("glm-5.1"));

    let r = resolve_routing(Some("claude-3-5-haiku-20241022"), &cfg2, None);
    chk("3 haiku→light", out(&r).as_deref() == Some("small-model"));

    let r = resolve_routing(Some("claude-sonnet-4-6"), &cfg2, None);
    chk("4 sonnet→primary", out(&r).as_deref() == Some("big-model"));

    let r = resolve_routing(Some("gpt-4-turbo"), &cfg2, None);
    chk("5 foreign→light", out(&r).as_deref() == Some("small-model"));

    let mut known = HashSet::new();
    known.insert("glm-5.2".to_string());
    let r = resolve_routing(Some("glm-5.2"), &cfg2, Some(&known));
    chk("6 known passthrough", out(&r).as_deref() == Some("glm-5.2"));

    let r = resolve_routing(Some("other-alias"), &cfg2, None);
    chk("7 stays on active", pidf(&r).as_deref() == Some("main") && out(&r).as_deref() == Some("small-model"));

    let r = resolve_routing(Some("whatever-x"), &off, None);
    chk("8 mapoff passthrough", out(&r).as_deref() == Some("whatever-x"));

    json!({ "total": n, "passed": n - fails.len(), "failed": fails.len(), "fails": fails })
}
