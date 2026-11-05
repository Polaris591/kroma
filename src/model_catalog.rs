//! Small, auditable fallback catalog for known provider/model pairs.
//!
//! This is not a remote model service or a capability guesser. Records are
//! exact `(provider, model id)` pairs from public provider documentation. A
//! custom proxy never inherits capabilities from a same-named official model;
//! it must use an explicit `catalog_model` / `KROMA_MODEL_CATALOG_MODEL` alias.
use serde::Serialize;
use std::fmt;

pub const CATALOG_SCHEMA_VERSION: u32 = 4;

/// Bump this when a record is added or corrected. The source and verification
/// date stay attached to each record so a future update can be audited.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CatalogRecord {
    pub provider: &'static str,
    pub model: &'static str,
    pub context_window: Option<usize>,
    /// Independent input-only ceiling, when documented separately from the window.
    pub max_input_tokens: Option<usize>,
    pub max_output_tokens: Option<usize>,
    /// None means unknown; it is never treated as false.
    pub image_input: Option<bool>,
    /// None means unknown, not "unrestricted".
    pub thinking_levels: Option<&'static [&'static str]>,
    pub source_url: &'static str,
    pub verified: &'static str,
}

impl CatalogRecord {
    pub fn key(&self) -> String {
        format!("{}/{}", self.provider, self.model)
    }
}

mod records;
use records::RECORDS;

/// Provider identity comes from the exact official HTTPS endpoint, not the
/// model name or a same-named local proxy.
pub fn provider_for_endpoint(base_url: &str) -> Option<&'static str> {
    let url = reqwest::Url::parse(base_url).ok()?;
    // A reverse proxy or hosts-file forward on another scheme/port is not the
    // official provider even when it reuses the same host name.
    if url.scheme() != "https" || url.port_or_known_default() != Some(443) {
        return None;
    }
    let host = url.host_str()?.to_ascii_lowercase();
    match host.as_str() {
        "api.deepseek.com" => Some("deepseek"),
        "api.anthropic.com" => Some("anthropic"),
        "api.openai.com" => Some("openai"),
        "generativelanguage.googleapis.com" => Some("google"),
        "api.x.ai" | "us.api.x.ai" => Some("xai"),
        "dashscope.aliyuncs.com" | "dashscope-intl.aliyuncs.com" | "dashscope-us.aliyuncs.com" => {
            Some("alibaba")
        }
        "api.moonshot.ai" => Some("moonshot"),
        "api.z.ai" | "open.bigmodel.cn" => Some("zhipu"),
        "api.minimax.io" => Some("minimax"),
        "api.mistral.ai" => Some("mistral"),
        "api.cohere.ai" => Some("cohere"),
        "ark.cn-beijing.volces.com" => Some("bytedance"),
        "api.nova.amazon.com" => Some("amazon"),
        "integrate.api.nvidia.com" => Some("nvidia"),
        "api.stepfun.com" => Some("stepfun"),
        "api.xiaomimimo.com" => Some("xiaomi"),
        "qianfan.baidubce.com" => Some("baidu"),
        "tokenhub.tencentmaas.com"
        | "tokenhub-intl.tencentmaas.com"
        | "api.hunyuan.cloud.tencent.com" => Some("tencent"),
        _ => None,
    }
}

pub fn find_exact(provider: &str, model: &str) -> Option<&'static CatalogRecord> {
    RECORDS
        .iter()
        .find(|record| record.provider == provider && record.model == model)
}

/// Explicit alias syntax is `provider/model-id` and is matched literally.
pub fn find_alias(alias: &str) -> Option<&'static CatalogRecord> {
    RECORDS.iter().find(|record| record.key() == alias)
}

pub fn select(
    base_url: &str,
    model: &str,
    explicit_alias: Option<&str>,
) -> anyhow::Result<Option<&'static CatalogRecord>> {
    if let Some(alias) = explicit_alias {
        let alias = alias.trim();
        return find_alias(alias)
            .map(Some)
            .ok_or_else(|| anyhow::anyhow!("unknown built-in model catalog alias '{alias}'"));
    }
    Ok(provider_for_endpoint(base_url).and_then(|provider| find_exact(provider, model)))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FieldSource {
    Environment,
    Profile,
    Upstream,
    Catalog,
    Default,
    Computed,
    Plugin,
    Unknown,
}

impl FieldSource {
    pub fn label(self) -> &'static str {
        match self {
            Self::Environment => "environment",
            Self::Profile => "profile",
            Self::Upstream => "upstream",
            Self::Catalog => "catalog",
            Self::Default => "default",
            Self::Computed => "computed",
            Self::Plugin => "plugin",
            Self::Unknown => "unknown",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MetadataState {
    NotQueried,
    Available,
    Unavailable,
}

impl MetadataState {
    pub fn label(self) -> &'static str {
        match self {
            Self::NotQueried => "not queried",
            Self::Available => "available",
            Self::Unavailable => "unavailable",
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct FieldValue<T> {
    pub value: Option<T>,
    pub source: FieldSource,
    pub note: Option<String>,
}

impl<T> FieldValue<T> {
    fn new(value: Option<T>, source: FieldSource, note: impl Into<Option<String>>) -> Self {
        Self {
            value,
            source,
            note: note.into(),
        }
    }

    fn unknown(note: impl Into<Option<String>>) -> Self {
        Self::new(None, FieldSource::Unknown, note)
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct CatalogMatch {
    pub key: String,
    pub provider: String,
    pub model: String,
    pub source_url: &'static str,
    pub verified: &'static str,
}

impl From<&CatalogRecord> for CatalogMatch {
    fn from(record: &CatalogRecord) -> Self {
        Self {
            key: record.key(),
            provider: record.provider.to_owned(),
            model: record.model.to_owned(),
            source_url: record.source_url,
            verified: record.verified,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct UpstreamLimits {
    pub context_window: Option<usize>,
    pub max_output_tokens: Option<usize>,
    pub image_input: Option<bool>,
}

#[derive(Debug, Clone)]
pub struct ModelInputs<'a> {
    pub model: &'a str,
    pub base_url: &'a str,
    pub context_window: Option<usize>,
    pub context_source: FieldSource,
    pub requested_output: Option<usize>,
    pub requested_output_source: FieldSource,
    pub thinking_levels: &'a [String],
    pub thinking_levels_source: FieldSource,
}

#[derive(Debug, Clone, Serialize)]
pub struct ModelReport {
    pub catalog_schema_version: u32,
    pub model: String,
    pub provider: String,
    pub endpoint_host: String,
    pub metadata: MetadataState,
    pub metadata_detail: Option<String>,
    pub catalog: Option<CatalogMatch>,
    pub context_window: FieldValue<usize>,
    pub hard_output_limit: FieldValue<usize>,
    pub requested_output_tokens: FieldValue<usize>,
    pub effective_turn_output_tokens: FieldValue<usize>,
    pub output_reserve: FieldValue<usize>,
    pub image_input: FieldValue<bool>,
    pub thinking_levels: FieldValue<Vec<String>>,
}

impl ModelReport {
    pub fn status_text(&self) -> String {
        let mut lines = vec![
            format!(
                "model={} | provider={} | host={}",
                self.model, self.provider, self.endpoint_host
            ),
            format!(
                "metadata={}{}",
                self.metadata.label(),
                self.metadata_detail
                    .as_deref()
                    .map(|detail| format!(" ({detail})"))
                    .unwrap_or_default()
            ),
        ];
        if let Some(catalog) = &self.catalog {
            lines.push(format!(
                "catalog={} | source={} | verified={}",
                catalog.key, catalog.source_url, catalog.verified
            ));
        }
        for (label, field) in [
            ("context", &self.context_window),
            ("hard_output", &self.hard_output_limit),
            ("requested_output", &self.requested_output_tokens),
            ("effective_turn_output", &self.effective_turn_output_tokens),
            ("output_reserve", &self.output_reserve),
        ] {
            lines.push(render_field(label, field, |value| value.to_string()));
        }
        lines.push(render_field("image_input", &self.image_input, |value| {
            value.to_string()
        }));
        lines.push(render_field(
            "thinking_levels",
            &self.thinking_levels,
            |levels| {
                if levels.is_empty() {
                    "all (no profile restriction)".to_owned()
                } else {
                    levels.join(",")
                }
            },
        ));
        lines.join("\n")
    }
}

fn render_field<T>(label: &str, field: &FieldValue<T>, value: impl FnOnce(&T) -> String) -> String {
    let value = field
        .value
        .as_ref()
        .map(value)
        .unwrap_or_else(|| "unknown".to_owned());
    let note = field
        .note
        .as_deref()
        .map(|note| format!("; {note}"))
        .unwrap_or_default();
    format!("{label}={value} (source={}{note})", field.source.label())
}

#[derive(Debug, Clone)]
pub struct ModelResolution {
    pub report: ModelReport,
    pub context_window: usize,
    pub effective_turn_output: Option<usize>,
    /// Input-only catalog ceiling; the shared context window remains separate.
    pub catalog_max_input_tokens: Option<usize>,
    pub catalog_max_output_tokens: Option<usize>,
    pub catalog_image_input: Option<bool>,
}

/// Resolve each field independently. Explicit context is capped by a known
/// endpoint/catalog maximum; output requests are capped by the known hard
/// maximum and Kroma's per-turn allowance.
pub fn resolve(
    inputs: &ModelInputs<'_>,
    upstream: &UpstreamLimits,
    metadata: MetadataState,
    metadata_detail: Option<String>,
    catalog: Option<&CatalogRecord>,
) -> ModelResolution {
    let endpoint_host = reqwest::Url::parse(inputs.base_url)
        .ok()
        .and_then(|url| url.host_str().map(str::to_owned))
        .unwrap_or_else(|| "(invalid)".to_owned());
    let provider = catalog
        .map(|record| record.provider.to_owned())
        .or_else(|| provider_for_endpoint(inputs.base_url).map(str::to_owned))
        .unwrap_or_else(|| endpoint_host.clone());

    let upstream_context = upstream.context_window;
    let catalog_context = catalog.and_then(|record| record.context_window);
    let (context_window, context_source, context_note) =
        if let Some(explicit) = inputs.context_window {
            match (upstream_context, catalog_context) {
                (Some(cap), _) if explicit > cap => (
                    cap,
                    FieldSource::Upstream,
                    Some(format!(
                        "explicit {explicit} capped to the endpoint maximum {cap}"
                    )),
                ),
                (None, Some(cap)) if explicit > cap => (
                    cap,
                    FieldSource::Catalog,
                    Some(format!(
                        "explicit {explicit} capped to the catalog maximum {cap}"
                    )),
                ),
                _ => (explicit, inputs.context_source, None),
            }
        } else if let Some(context) = upstream_context {
            (context, FieldSource::Upstream, None)
        } else if let Some(context) = catalog_context {
            (context, FieldSource::Catalog, None)
        } else {
            (128_000, FieldSource::Default, None)
        };

    let catalog_max_input_tokens = catalog.and_then(|record| record.max_input_tokens);
    let context_note = match (context_note, catalog_max_input_tokens) {
        (note, Some(limit)) => {
            let input_note = format!("independent catalog input ceiling {limit}");
            Some(note.map_or_else(
                || input_note.clone(),
                |note| format!("{note}; {input_note}"),
            ))
        }
        (note, None) => note,
    };

    let (hard_output, hard_output_source) = if let Some(output) = upstream.max_output_tokens {
        (Some(output), FieldSource::Upstream)
    } else if let Some(output) = catalog.and_then(|record| record.max_output_tokens) {
        (Some(output), FieldSource::Catalog)
    } else {
        (None, FieldSource::Unknown)
    };

    let requested = inputs.requested_output;
    let (effective_turn_output, effective_note): (Option<usize>, Option<String>) = match requested {
        Some(requested) => {
            let mut capped = requested.min(crate::provider::MAX_TURN_OUTPUT_TOKENS);
            let mut notes = Vec::new();
            if capped < requested {
                notes.push(format!(
                    "per-turn cap {}",
                    crate::provider::MAX_TURN_OUTPUT_TOKENS
                ));
            }
            if let Some(limit) = hard_output {
                if capped > limit {
                    capped = limit;
                    notes.push(format!("hard cap {limit}"));
                }
            }
            let note = (!notes.is_empty()).then(|| format!("min({})", notes.join(", ")));
            (Some(capped), note)
        }
        None => match hard_output {
            Some(limit) => {
                let planning = limit.min(4096);
                (
                    Some(planning),
                    Some(format!(
                        "provider default; {planning} planning allowance from hard cap {limit}"
                    )),
                )
            }
            None => (None, None),
        },
    };

    let (image_input, image_source) = if let Some(value) = upstream.image_input {
        (Some(value), FieldSource::Upstream)
    } else if let Some(value) = catalog.and_then(|record| record.image_input) {
        (Some(value), FieldSource::Catalog)
    } else {
        (None, FieldSource::Unknown)
    };

    let (thinking_levels, thinking_source) =
        if inputs.thinking_levels_source != FieldSource::Unknown {
            (
                Some(inputs.thinking_levels.to_vec()),
                inputs.thinking_levels_source,
            )
        } else if let Some(levels) = catalog.and_then(|record| record.thinking_levels) {
            (
                Some(levels.iter().map(|level| (*level).to_owned()).collect()),
                FieldSource::Catalog,
            )
        } else {
            (None, FieldSource::Unknown)
        };

    let report = ModelReport {
        catalog_schema_version: CATALOG_SCHEMA_VERSION,
        model: inputs.model.to_owned(),
        provider,
        endpoint_host,
        metadata,
        metadata_detail,
        catalog: catalog.map(CatalogMatch::from),
        context_window: FieldValue::new(Some(context_window), context_source, context_note),
        hard_output_limit: FieldValue::new(
            hard_output,
            hard_output_source,
            (hard_output.is_none()).then(|| "no provider hard maximum known".to_owned()),
        ),
        requested_output_tokens: FieldValue::new(
            requested,
            if requested.is_some() {
                inputs.requested_output_source
            } else {
                FieldSource::Unknown
            },
            (requested.is_none())
                .then(|| "provider default; Kroma sends no explicit cap".to_owned()),
        ),
        effective_turn_output_tokens: FieldValue::new(
            effective_turn_output,
            if effective_turn_output.is_some() {
                FieldSource::Computed
            } else {
                FieldSource::Unknown
            },
            effective_note,
        ),
        output_reserve: FieldValue::new(None, FieldSource::Unknown, None),
        image_input: FieldValue::new(
            image_input,
            image_source,
            (image_input.is_none()).then(|| "capability unknown; not treated as false".to_owned()),
        ),
        thinking_levels: FieldValue::new(
            thinking_levels,
            thinking_source,
            (thinking_source == FieldSource::Unknown).then(|| {
                "provider tiers unknown; all configured levels remain available".to_owned()
            }),
        ),
    };
    ModelResolution {
        report,
        context_window,
        effective_turn_output,
        catalog_max_input_tokens,
        catalog_max_output_tokens: catalog.and_then(|record| record.max_output_tokens),
        catalog_image_input: catalog.and_then(|record| record.image_input),
    }
}

pub fn finish_report(
    mut report: ModelReport,
    reserve: usize,
    reserve_source: FieldSource,
) -> ModelReport {
    report.output_reserve = FieldValue::new(
        Some(reserve),
        reserve_source,
        Some("reserved from the context window for completion".to_owned()),
    );
    report
}

/// Completion budget the wire request will actually use for planning.
/// Responses and Anthropic always send an explicit limit and default to 4096;
/// Chat may omit `max_tokens` unless the profile requested one.
pub fn effective_wire_output(protocol: &str, resolved: Option<usize>) -> Option<usize> {
    if protocol == "chat" {
        resolved
    } else {
        Some(resolved.unwrap_or(4096))
    }
}

/// Keep the offline diagnostics report consistent with the runtime. Native
/// protocols replace the resolved planning value with the concrete wire value.
pub fn annotate_wire_output(
    mut report: ModelReport,
    protocol: &str,
    resolved: Option<usize>,
) -> ModelReport {
    if protocol != "chat" {
        report.effective_turn_output_tokens.value = Some(resolved.unwrap_or(4096));
        report.effective_turn_output_tokens.source = FieldSource::Computed;
        report.effective_turn_output_tokens.note =
            Some("actual native request max_output_tokens/max_tokens".into());
    }
    report
}

/// A process-model plugin owns its provider contract. This report describes
/// only what the host can verify without contacting the plugin.
pub fn plugin_report(
    model: &str,
    context_window: usize,
    output_reserve: usize,
    image_input: Option<bool>,
    thinking_levels: &[String],
) -> ModelReport {
    ModelReport {
        catalog_schema_version: CATALOG_SCHEMA_VERSION,
        model: model.to_owned(),
        provider: "plugin".into(),
        endpoint_host: "plugin".into(),
        metadata: MetadataState::NotQueried,
        metadata_detail: Some("provider contract is owned by the plugin".into()),
        catalog: None,
        context_window: FieldValue::new(
            Some(context_window),
            FieldSource::Plugin,
            Some("declared by plugin manifest".to_owned()),
        ),
        hard_output_limit: FieldValue::unknown(Some(
            "process plugin does not declare a hard output maximum".to_owned(),
        )),
        requested_output_tokens: FieldValue::unknown(Some(
            "provider defaults are owned by the plugin".to_owned(),
        )),
        effective_turn_output_tokens: FieldValue::unknown(Some(
            "provider defaults are owned by the plugin".to_owned(),
        )),
        output_reserve: FieldValue::new(
            Some(output_reserve),
            FieldSource::Plugin,
            Some("declared by plugin manifest".to_owned()),
        ),
        image_input: FieldValue::new(
            image_input,
            if image_input.is_some() {
                FieldSource::Plugin
            } else {
                FieldSource::Unknown
            },
            (image_input.is_none()).then(|| "plugin did not declare image support".to_owned()),
        ),
        thinking_levels: FieldValue::new(
            (!thinking_levels.is_empty()).then(|| thinking_levels.to_vec()),
            if thinking_levels.is_empty() {
                FieldSource::Unknown
            } else {
                FieldSource::Plugin
            },
            thinking_levels
                .is_empty()
                .then(|| "plugin did not declare thinking tiers".to_owned()),
        ),
    }
}

impl fmt::Display for ModelReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.status_text())
    }
}

/// Suggested model IDs for an interactive setup picker. Sorted by advertised
/// context capacity, then output capacity; this is a convenience list, not a
/// recommendation or an access guarantee.
pub fn suggestions(provider: &str, limit: usize) -> Vec<&'static CatalogRecord> {
    let mut records: Vec<_> = RECORDS
        .iter()
        .filter(|record| record.provider == provider)
        .collect();
    records.sort_by(|a, b| {
        b.context_window
            .cmp(&a.context_window)
            .then_with(|| b.max_output_tokens.cmp(&a.max_output_tokens))
            .then_with(|| a.model.cmp(b.model))
    });
    records.truncate(limit);
    records
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_official_endpoints_match_only_exact_provider_and_model_pairs() {
        for (host, model, provider) in [
            ("api.stepfun.com", "step-5-preview", "stepfun"),
            ("api.xiaomimimo.com", "mimo-v2.6-pro", "xiaomi"),
            ("qianfan.baidubce.com", "ernie-5.1", "baidu"),
            ("tokenhub.tencentmaas.com", "hy4", "tencent"),
            ("tokenhub-intl.tencentmaas.com", "hy3", "tencent"),
            ("api.hunyuan.cloud.tencent.com", "hunyuan-a13b", "tencent"),
            ("api.openai.com", "gpt-6-astra", "openai"),
            (
                "generativelanguage.googleapis.com",
                "gemini-4-argon",
                "google",
            ),
            ("api.x.ai", "grok-4.7", "xai"),
            ("us.api.x.ai", "grok-4.7", "xai"),
            ("dashscope.aliyuncs.com", "qwen3.8-max", "alibaba"),
            ("dashscope-intl.aliyuncs.com", "qwen3.8-max", "alibaba"),
            ("dashscope-us.aliyuncs.com", "qwen3.8-max", "alibaba"),
            ("api.moonshot.ai", "kimi-k3.1-pro", "moonshot"),
            ("api.z.ai", "glm-5.3", "zhipu"),
            ("open.bigmodel.cn", "glm-5.3", "zhipu"),
            ("api.minimax.io", "MiniMax-M3.1-Flash", "minimax"),
            ("api.mistral.ai", "mistral-large-4", "mistral"),
            ("api.cohere.ai", "command-a-plus-05-2026", "cohere"),
            (
                "ark.cn-beijing.volces.com",
                "doubao-seed-evolving",
                "bytedance",
            ),
            ("api.nova.amazon.com", "nova-2-pro-v1", "amazon"),
            (
                "integrate.api.nvidia.com",
                "nvidia/nemotron-3.5-super-vl-120b-a12b",
                "nvidia",
            ),
        ] {
            let url = format!("https://{host}/v1");
            let found = select(&url, model, None).unwrap().unwrap();
            assert_eq!(found.provider, provider, "{url}");
            assert_eq!(find_alias(&found.key()), Some(found));
            assert!(select(&url, "unknown-future-model", None)
                .unwrap()
                .is_none());
            for url in [
                format!("http://{host}/v1"),
                format!("https://{host}:444/v1"),
                format!("https://{host}.proxy.example/v1"),
                format!("https://{host}@proxy.example/v1"),
            ] {
                assert!(select(&url, model, None).unwrap().is_none(), "{url}");
            }
            assert!(select("https://proxy.example/v1", model, None)
                .unwrap()
                .is_none());
            assert_eq!(
                select("https://proxy.example/v1", "renamed", Some(&found.key())).unwrap(),
                Some(found)
            );
        }
        // No wildcard vendor/CDN hosts, managed-cloud auth, or fuzzy model IDs.
        assert!(provider_for_endpoint("https://bedrock-runtime.us-east-1.amazonaws.com").is_none());
        assert!(provider_for_endpoint("https://workspace.cn-beijing.maas.aliyuncs.com").is_none());
        assert!(find_exact("openai", "gpt-6-astra-unreleased").is_none());
        assert!(find_exact("minimax", "minimax-m3.1-flash").is_none());
    }

    #[test]
    fn flagship_limits_do_not_confuse_defaults_with_hard_maxima() {
        for alias in ["zhipu/glm-5.3-flash", "zhipu/glm-5.3-flashx"] {
            let record = find_alias(alias).unwrap();
            assert_eq!(record.image_input, Some(true));
            assert_eq!(record.max_output_tokens, Some(128_000));
        }
        let nvidia = find_alias("nvidia/nvidia/nemotron-3.5-super-vl-120b-a12b").unwrap();
        assert_eq!(nvidia.model, "nvidia/nemotron-3.5-super-vl-120b-a12b");
        assert_eq!(
            nvidia.max_output_tokens, None,
            "an example max_tokens is not a hard cap"
        );
        assert_eq!(
            nvidia.thinking_levels, None,
            "enable_thinking is not reasoning_effort"
        );
        let astra = find_alias("openai/gpt-6-astra").unwrap();
        assert_eq!(astra.context_window, Some(1_050_000));
        assert_eq!(astra.max_input_tokens, Some(922_000));
        assert_eq!(astra.max_output_tokens, Some(128_000));
        assert_eq!(
            find_alias("google/gemini-4-argon")
                .unwrap()
                .max_output_tokens,
            Some(65_536),
            "foreground streaming, not background long decode"
        );
        assert_eq!(
            find_alias("moonshot/kimi-k3.1-pro")
                .unwrap()
                .max_output_tokens,
            Some(1_048_576),
            "the 131072 default is not the hard maximum"
        );
        assert_eq!(
            find_alias("minimax/MiniMax-M3.1-Flash")
                .unwrap()
                .max_output_tokens,
            Some(524_288)
        );
        for alias in [
            "xai/grok-4.7",
            "mistral/mistral-large-4",
            "amazon/nova-2-lite-v1",
        ] {
            assert_eq!(
                find_alias(alias).unwrap().max_output_tokens,
                None,
                "{alias}: unstated or conflicting maximum stays unknown"
            );
        }
        assert_eq!(
            find_alias("zhipu/glm-5.3").unwrap().image_input,
            Some(false)
        );
        assert_eq!(
            find_alias("minimax/MiniMax-M3").unwrap().thinking_levels,
            None,
            "ignored effort is not supported effort"
        );
        assert_eq!(
            find_alias("bytedance/doubao-seed-evolving")
                .unwrap()
                .context_window,
            Some(1_024_000),
            "shorthand-only context uses a conservative decimal bound"
        );
    }

    #[test]
    fn aliases_share_verified_caps_but_unlisted_snapshots_do_not_inherit() {
        for (provider, alias, pinned) in [
            ("openai", "gpt-5", "gpt-5-2025-08-07"),
            ("openai", "gpt-5.1", "gpt-5.1-2025-11-13"),
            ("openai", "gpt-5.6", "gpt-5.6-sol"),
            ("anthropic", "claude-haiku-4-5", "claude-haiku-4-5-20251001"),
            ("alibaba", "qwen-plus", "qwen-plus-2025-12-01"),
            ("alibaba", "qwen3-coder-plus", "qwen3-coder-plus-2025-09-23"),
        ] {
            let a = find_exact(provider, alias).unwrap();
            let b = find_exact(provider, pinned).unwrap();
            assert_eq!(a.context_window, b.context_window, "{alias}");
            assert_eq!(a.max_input_tokens, b.max_input_tokens, "{alias}");
            assert_eq!(a.max_output_tokens, b.max_output_tokens, "{alias}");
            assert_eq!(a.image_input, b.image_input, "{alias}");
            assert_eq!(a.thinking_levels, b.thinking_levels, "{alias}");
            assert_eq!(a.source_url, b.source_url, "{alias}");
            assert!(find_exact(provider, &format!("{alias}-2099-01-01")).is_none());
        }
        for (provider, retired) in [
            ("deepseek", "deepseek-chat"),
            ("deepseek", "deepseek-reasoner"),
            ("openai", "gpt-4o-2024-05-13"),
            ("google", "gemini-2.0-flash"),
            ("anthropic", "claude-opus-4-1-20250805"),
            ("xiaomi", "mimo-v2.5-pro"),
        ] {
            assert!(find_exact(provider, retired).is_none(), "{retired}");
        }
    }

    #[test]
    fn expanded_families_keep_per_model_budgets_and_effort_semantics() {
        let pro = find_exact("openai", "gpt-5-pro").unwrap();
        assert_eq!(pro.context_window, Some(400_000));
        assert_eq!(pro.max_output_tokens, Some(272_000));
        assert_eq!(pro.thinking_levels, Some(&["high"][..]));
        assert_eq!(
            find_exact("openai", "gpt-4.1").unwrap().context_window,
            Some(1_047_576)
        );
        assert_eq!(
            find_exact("openai", "gpt-4o").unwrap().max_output_tokens,
            Some(16_384)
        );
        assert_eq!(
            find_exact("alibaba", "qwen-plus").unwrap().max_input_tokens,
            Some(995_904)
        );
        assert_eq!(
            find_exact("alibaba", "qwen3-vl-plus")
                .unwrap()
                .max_input_tokens,
            Some(258_048)
        );
        assert_eq!(
            find_exact("google", "gemini-4-argon")
                .unwrap()
                .thinking_levels,
            Some(&["low", "medium", "high"][..])
        );
        assert_eq!(
            find_exact("openai", "gpt-6.1-terra")
                .unwrap()
                .thinking_levels
                .unwrap()
                .first(),
            Some(&"off")
        );
        assert!(!find_exact("openai", "gpt-6-astra")
            .unwrap()
            .thinking_levels
            .unwrap()
            .contains(&"off"));
        let haiku = find_exact("anthropic", "claude-haiku-4-5").unwrap();
        assert_eq!(haiku.max_output_tokens, Some(64_000));
        assert_eq!(
            haiku.thinking_levels, None,
            "manual thinking is not effort tiers"
        );
        let hy4 = find_exact("tencent", "hy4").unwrap();
        assert_eq!(
            hy4.context_window, None,
            "IDs-only source does not establish caps"
        );
        assert_eq!(hy4.max_output_tokens, None);
        assert_eq!(hy4.image_input, None);
        assert_eq!(hy4.thinking_levels, None);
        assert!(
            find_exact("baidu", "ernie-5.0").is_none(),
            "stale search extract absent from current directory"
        );
    }

    #[test]
    fn catalog_keys_and_numeric_limits_are_consistent() {
        let mut keys = std::collections::HashSet::new();
        for record in RECORDS {
            assert!(keys.insert(record.key()), "duplicate {}", record.key());
            if let Some(input) = record.max_input_tokens {
                assert!(input >= 1024);
                assert!(record.context_window.is_some_and(|window| input <= window));
            }
            if let Some(output) = record.max_output_tokens {
                assert!(output > 0);
            }
            if let Some(levels) = record.thinking_levels {
                let strings: Vec<_> = levels.iter().map(|level| (*level).to_owned()).collect();
                assert_eq!(
                    crate::profiles::normalize_levels(&strings).unwrap(),
                    strings,
                    "{} uses canonical Kroma levels",
                    record.key()
                );
            }
        }
        assert_eq!(keys.len(), 110);
        let providers: std::collections::HashSet<_> = RECORDS.iter().map(|r| r.provider).collect();
        assert_eq!(providers.len(), 18);
    }

    fn fixture() -> CatalogRecord {
        CatalogRecord {
            provider: "fixture-provider",
            model: "fixture-model-v1",
            context_window: Some(500_000),
            max_input_tokens: None,
            max_output_tokens: Some(64_000),
            image_input: Some(true),
            thinking_levels: Some(&["low", "high"]),
            source_url: "https://fixture.invalid/model-docs",
            verified: "2030-01-01",
        }
    }

    fn inputs(levels: &[String]) -> ModelInputs<'_> {
        ModelInputs {
            model: "fixture-model-v1",
            base_url: "https://proxy.example/v1",
            context_window: None,
            context_source: FieldSource::Unknown,
            requested_output: None,
            requested_output_source: FieldSource::Unknown,
            thinking_levels: levels,
            thinking_levels_source: FieldSource::Unknown,
        }
    }

    #[test]
    fn fields_fall_back_independently_from_catalog() {
        let catalog = fixture();
        let upstream = UpstreamLimits {
            context_window: Some(200_000),
            ..Default::default()
        };
        let resolved = resolve(
            &inputs(&[]),
            &upstream,
            MetadataState::Available,
            None,
            Some(&catalog),
        );
        assert_eq!(resolved.context_window, 200_000);
        assert_eq!(resolved.report.context_window.source, FieldSource::Upstream);
        assert_eq!(
            resolved.report.hard_output_limit.value,
            Some(64_000),
            "missing upstream output limit uses the catalog field"
        );
        assert_eq!(
            resolved.report.hard_output_limit.source,
            FieldSource::Catalog
        );
        assert_eq!(resolved.report.image_input.value, Some(true));
        assert_eq!(
            resolved.report.thinking_levels.value.as_deref(),
            Some(&["low".to_owned(), "high".to_owned()][..])
        );
        assert_eq!(resolved.effective_turn_output, Some(4096));
    }

    #[test]
    fn explicit_values_are_kept_but_hard_caps_are_not_bypassed() {
        let catalog = fixture();
        let mut input = inputs(&[]);
        input.context_window = Some(400_000);
        input.context_source = FieldSource::Profile;
        input.requested_output = Some(100_000);
        input.requested_output_source = FieldSource::Environment;
        let resolved = resolve(
            &input,
            &UpstreamLimits::default(),
            MetadataState::NotQueried,
            None,
            Some(&catalog),
        );
        assert_eq!(resolved.context_window, 400_000);
        assert_eq!(resolved.report.context_window.source, FieldSource::Profile);
        assert_eq!(resolved.report.requested_output_tokens.value, Some(100_000));
        assert_eq!(
            resolved.report.requested_output_tokens.source,
            FieldSource::Environment
        );
        assert_eq!(
            resolved.effective_turn_output,
            Some(crate::provider::MAX_TURN_OUTPUT_TOKENS)
        );
        input.context_window = Some(900_000);
        let capped = resolve(
            &input,
            &UpstreamLimits::default(),
            MetadataState::NotQueried,
            None,
            Some(&catalog),
        );
        assert_eq!(capped.context_window, 500_000);
        assert_eq!(capped.report.context_window.source, FieldSource::Catalog);
        assert!(capped
            .report
            .context_window
            .note
            .unwrap()
            .contains("capped"));
    }

    #[test]
    fn unknown_models_and_images_remain_usable_and_unknown() {
        let resolved = resolve(
            &inputs(&[]),
            &UpstreamLimits::default(),
            MetadataState::Unavailable,
            Some("timeout".into()),
            None,
        );
        assert_eq!(resolved.context_window, 128_000);
        assert_eq!(resolved.report.context_window.source, FieldSource::Default);
        assert_eq!(resolved.report.image_input.value, None);
        assert_eq!(resolved.report.image_input.source, FieldSource::Unknown);
        assert_eq!(resolved.report.hard_output_limit.value, None);
    }

    #[test]
    fn image_capability_keeps_three_states_and_upstream_wins() {
        let vision = fixture();
        let false_catalog = CatalogRecord {
            image_input: Some(false),
            ..vision
        };
        let unknown_catalog = CatalogRecord {
            image_input: None,
            ..vision
        };
        let unknown = UpstreamLimits::default();
        let explicit_false = UpstreamLimits {
            image_input: Some(false),
            ..Default::default()
        };
        let case = |upstream: &UpstreamLimits, catalog: &CatalogRecord| {
            resolve(
                &inputs(&[]),
                upstream,
                MetadataState::Available,
                None,
                Some(catalog),
            )
            .report
            .image_input
        };
        assert_eq!(case(&unknown, &vision).value, Some(true));
        assert_eq!(case(&unknown, &false_catalog).value, Some(false));
        assert_eq!(case(&unknown, &unknown_catalog).value, None);
        assert_eq!(
            case(&unknown, &unknown_catalog).source,
            FieldSource::Unknown
        );
        // An endpoint that explicitly says text-only is not overridden by the
        // catalog's official capability.
        assert_eq!(case(&explicit_false, &vision).value, Some(false));
        assert_eq!(case(&explicit_false, &vision).source, FieldSource::Upstream);
    }

    #[test]
    fn custom_hosts_do_not_inherit_official_capabilities_by_name() {
        assert!(select("https://proxy.example/v1", "deepseek-flash", None)
            .unwrap()
            .is_none());
        assert_eq!(
            select("https://api.deepseek.com", "deepseek-flash", None)
                .unwrap()
                .unwrap()
                .key(),
            "deepseek/deepseek-flash"
        );
        assert!(select(
            "https://api.deepseek.com.proxy.example",
            "deepseek-flash",
            None
        )
        .unwrap()
        .is_none());
        assert_eq!(
            select(
                "https://proxy.example/v1",
                "renamed-model",
                Some("deepseek/deepseek-flash")
            )
            .unwrap()
            .unwrap()
            .key(),
            "deepseek/deepseek-flash"
        );
        assert!(select("https://proxy.example/v1", "x", Some("missing/model")).is_err());
    }

    #[test]
    fn official_catalog_requires_the_official_https_endpoint() {
        assert_eq!(
            provider_for_endpoint("https://api.deepseek.com/v1"),
            Some("deepseek")
        );
        assert_eq!(
            provider_for_endpoint("https://API.ANTHROPIC.COM:443/v1"),
            Some("anthropic")
        );
        for endpoint in [
            "http://api.deepseek.com/v1",
            "https://api.deepseek.com:444/v1",
            "https://api.deepseek.com.local/v1",
            "https://api.deepseek.com@proxy.example/v1",
            "https://proxy.example/api.deepseek.com",
        ] {
            assert!(
                provider_for_endpoint(endpoint).is_none(),
                "{endpoint} must not inherit official capabilities"
            );
            assert!(
                select(endpoint, "deepseek-flash", None).unwrap().is_none(),
                "{endpoint} must not select a catalog record"
            );
        }
    }

    #[test]
    fn bundled_records_carry_verifiable_sources_and_exact_keys() {
        for record in RECORDS {
            assert!(record.source_url.starts_with("https://"));
            assert_eq!(record.verified.len(), 10);
            assert!(find_exact(record.provider, record.model).is_some());
            assert!(find_alias(&record.key()).is_some());
        }
    }
}
