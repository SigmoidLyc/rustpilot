//! Compact model capability and request-parameter catalog.
//!
//! This mirrors OpenCode's split between model capabilities and model variants:
//! a variant is a selectable reasoning level whose wire representation depends
//! on the provider dialect. Unknown models deliberately receive no inferred
//! variant, so a custom endpoint cannot be sent an unsupported parameter.

use serde::Serialize;
use serde_json::{json, Value};

use crate::llm::ReasoningEffort;

const EMPTY_EFFORTS: &[ReasoningEffort] = &[];
const WIDELY_SUPPORTED_EFFORTS: &[ReasoningEffort] = &[
    ReasoningEffort::Low,
    ReasoningEffort::Medium,
    ReasoningEffort::High,
];
const OPENAI_O_EFFORTS: &[ReasoningEffort] = WIDELY_SUPPORTED_EFFORTS;
const OPENAI_GPT5_BASE_EFFORTS: &[ReasoningEffort] = &[
    ReasoningEffort::Minimal,
    ReasoningEffort::Low,
    ReasoningEffort::Medium,
    ReasoningEffort::High,
];
const OPENAI_GPT5_1_EFFORTS: &[ReasoningEffort] = &[
    ReasoningEffort::None,
    ReasoningEffort::Low,
    ReasoningEffort::Medium,
    ReasoningEffort::High,
];
const OPENAI_GPT5_2_PLUS_EFFORTS: &[ReasoningEffort] = &[
    ReasoningEffort::None,
    ReasoningEffort::Low,
    ReasoningEffort::Medium,
    ReasoningEffort::High,
    ReasoningEffort::XHigh,
];
const OPENAI_GPT5_PRO_EFFORTS: &[ReasoningEffort] = &[ReasoningEffort::High];
const OPENAI_GPT5_PRO_2_PLUS_EFFORTS: &[ReasoningEffort] = &[
    ReasoningEffort::Medium,
    ReasoningEffort::High,
    ReasoningEffort::XHigh,
];
const OPENAI_GPT5_CODEX_XHIGH_EFFORTS: &[ReasoningEffort] = &[
    ReasoningEffort::Low,
    ReasoningEffort::Medium,
    ReasoningEffort::High,
    ReasoningEffort::XHigh,
];
const OPENAI_GPT5_CODEX_3_PLUS_EFFORTS: &[ReasoningEffort] = &[
    ReasoningEffort::None,
    ReasoningEffort::Low,
    ReasoningEffort::Medium,
    ReasoningEffort::High,
    ReasoningEffort::XHigh,
];
// Codex declares this exact tier set for GPT-5.6 Terra. Keep this separate
// from version-wide GPT-5 defaults: their supported levels are not uniform.
const OPENAI_GPT56_TERRA_EFFORTS: &[ReasoningEffort] = &[
    ReasoningEffort::Low,
    ReasoningEffort::Medium,
    ReasoningEffort::High,
    ReasoningEffort::XHigh,
    ReasoningEffort::Max,
    ReasoningEffort::Ultra,
];
const DEEPSEEK_V4_EFFORTS: &[ReasoningEffort] = &[
    ReasoningEffort::Off,
    ReasoningEffort::High,
    ReasoningEffort::Max,
];
// OpenRouter normalizes this model family separately from the direct
// DeepSeek endpoint. Its model directory advertises only these two levels.
const OPENROUTER_DEEPSEEK_V4_EFFORTS: &[ReasoningEffort] =
    &[ReasoningEffort::High, ReasoningEffort::XHigh];
const GLM52_COMPATIBLE_EFFORTS: &[ReasoningEffort] = &[ReasoningEffort::High, ReasoningEffort::Max];
const GLM52_OPENROUTER_EFFORTS: &[ReasoningEffort] =
    &[ReasoningEffort::High, ReasoningEffort::XHigh];
const GROK3_MINI_EFFORTS: &[ReasoningEffort] = &[ReasoningEffort::Low, ReasoningEffort::High];
const NORTH_MINI_CODE_EFFORTS: &[ReasoningEffort] = &[ReasoningEffort::None, ReasoningEffort::High];
const MISTRAL_REASONING_EFFORTS: &[ReasoningEffort] = &[ReasoningEffort::High];
const ALIBABA_TOGGLE_EFFORTS: &[ReasoningEffort] = &[ReasoningEffort::Off, ReasoningEffort::High];

#[derive(Debug, Clone, Serialize)]
pub struct ModelCapabilities {
    pub id: String,
    pub name: String,
    pub capabilities: ModelCapabilityFlags,
    pub reasoning_options: Vec<ReasoningOption>,
    pub variants: Vec<ModelVariant>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ModelCapabilityFlags {
    pub temperature: bool,
    pub reasoning: bool,
    pub attachment: bool,
    pub tool_call: bool,
    pub input: Vec<String>,
    pub output: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ReasoningOption {
    Effort { values: Vec<ReasoningEffort> },
}

#[derive(Debug, Clone, Serialize)]
pub struct ModelVariant {
    pub id: ReasoningEffort,
    pub name: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProviderFamily {
    OpenAi,
    OpenRouter,
    DeepSeek,
    Alibaba,
    Copilot,
    GenericOpenAiCompatible,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReasoningWire {
    None,
    OpenAiReasoningEffort,
    OpenRouterReasoning,
    DeepSeekV4,
    AlibabaThinkingToggle,
}

#[derive(Debug, Clone, Copy)]
struct ResolvedModel {
    reasoning: bool,
    temperature: bool,
    attachment: bool,
    tool_call: bool,
    efforts: &'static [ReasoningEffort],
    reasoning_wire: ReasoningWire,
    max_completion_tokens: bool,
}

impl ResolvedModel {
    fn unknown(model: &str) -> Self {
        Self {
            reasoning: false,
            temperature: true,
            attachment: model_supports_images(model),
            tool_call: true,
            efforts: EMPTY_EFFORTS,
            reasoning_wire: ReasoningWire::None,
            max_completion_tokens: false,
        }
    }

    fn reasoning(
        model: &str,
        efforts: &'static [ReasoningEffort],
        reasoning_wire: ReasoningWire,
        max_completion_tokens: bool,
    ) -> Self {
        Self {
            reasoning: true,
            temperature: !max_completion_tokens,
            attachment: model_supports_images(model),
            tool_call: true,
            efforts,
            reasoning_wire,
            max_completion_tokens,
        }
    }

    fn fixed_reasoning(model: &str) -> Self {
        Self {
            reasoning: true,
            temperature: true,
            attachment: model_supports_images(model),
            tool_call: true,
            efforts: EMPTY_EFFORTS,
            reasoning_wire: ReasoningWire::None,
            max_completion_tokens: false,
        }
    }
}

/// Returns the catalog-derived UI model information. It is intentionally
/// allocation-free on the request path; allocations here only prepare a Tauri
/// response for the model selector.
pub fn capabilities(model: &str, base_url: &str, api_type: &str) -> ModelCapabilities {
    let resolved = resolve(model, base_url, api_type);
    let id = model.trim().to_string();
    let mut input = vec!["text".to_string()];
    if resolved.attachment {
        input.push("image".to_string());
    }
    let variants = resolved
        .efforts
        .iter()
        .copied()
        .map(|id| ModelVariant {
            id,
            name: id.display_name().to_string(),
        })
        .collect::<Vec<_>>();
    let reasoning_options = if resolved.efforts.is_empty() {
        Vec::new()
    } else {
        vec![ReasoningOption::Effort {
            values: resolved.efforts.to_vec(),
        }]
    };

    ModelCapabilities {
        name: if id.is_empty() {
            "Model".to_string()
        } else {
            id.clone()
        },
        id,
        capabilities: ModelCapabilityFlags {
            temperature: resolved.temperature,
            reasoning: resolved.reasoning,
            attachment: resolved.attachment,
            tool_call: resolved.tool_call,
            input,
            output: vec!["text".to_string()],
        },
        reasoning_options,
        variants,
    }
}

pub(crate) fn reasoning_efforts_for_model(
    model: &str,
    base_url: &str,
    api_type: &str,
) -> &'static [ReasoningEffort] {
    resolve(model, base_url, api_type).efforts
}

pub(crate) fn validate_reasoning_effort(
    model: &str,
    base_url: &str,
    api_type: &str,
    effort: ReasoningEffort,
) -> Result<(), String> {
    if reasoning_efforts_for_model(model, base_url, api_type).contains(&effort) {
        Ok(())
    } else {
        Err(format!(
            "Model `{model}` does not support reasoning effort `{}`.",
            effort.as_str()
        ))
    }
}

pub(crate) fn apply_reasoning_effort(
    body: &mut Value,
    model: &str,
    base_url: &str,
    api_type: &str,
    effort: ReasoningEffort,
) -> Result<(), String> {
    let resolved = resolve(model, base_url, api_type);
    if !resolved.efforts.contains(&effort) {
        return validate_reasoning_effort(model, base_url, api_type, effort);
    }

    match resolved.reasoning_wire {
        ReasoningWire::None => Ok(()),
        ReasoningWire::OpenAiReasoningEffort => {
            body["reasoning_effort"] = Value::String(effort.as_str().to_string());
            Ok(())
        }
        ReasoningWire::OpenRouterReasoning => {
            body["reasoning"] = json!({"effort": openrouter_effort(effort)});
            Ok(())
        }
        ReasoningWire::DeepSeekV4 => match effort {
            ReasoningEffort::Off => {
                body["thinking"] = json!({"type": "disabled"});
                if let Some(object) = body.as_object_mut() {
                    object.remove("reasoning_effort");
                }
                Ok(())
            }
            ReasoningEffort::High | ReasoningEffort::Max => {
                body["thinking"] = json!({"type": "enabled"});
                body["reasoning_effort"] = Value::String(effort.as_str().to_string());
                Ok(())
            }
            _ => validate_reasoning_effort(model, base_url, api_type, effort),
        },
        ReasoningWire::AlibabaThinkingToggle => {
            body["enable_thinking"] = Value::Bool(!matches!(
                effort,
                ReasoningEffort::None | ReasoningEffort::Off
            ));
            Ok(())
        }
    }
}

pub(crate) fn uses_max_completion_tokens(model: &str, base_url: &str, api_type: &str) -> bool {
    resolve(model, base_url, api_type).max_completion_tokens
}

/// Returns the catalog's conservative context-window profile.
///
/// Providers do not expose a portable model-metadata endpoint through the
/// compatible chat API. Known families therefore use documented ceilings,
/// while custom endpoints stay conservative unless the operator supplies an
/// explicit `RUSTPILOT_CONTEXT_WINDOW` override.
pub(crate) fn context_window_for_model(model: &str, base_url: &str, api_type: &str) -> usize {
    const DEFAULT: usize = 32 * 1024;
    const OPENAI_LARGE: usize = 128 * 1024;
    const CLAUDE_LARGE: usize = 200 * 1024;
    const GEMINI_LARGE: usize = 1_000 * 1024;

    if let Ok(value) = std::env::var("RUSTPILOT_CONTEXT_WINDOW") {
        if let Ok(value) = value.trim().parse::<usize>() {
            if (8 * 1024..=2 * 1024 * 1024).contains(&value) {
                return value;
            }
        }
    }

    let id = model_id(model);
    if contains_ascii_case_insensitive(id, "gemini") {
        return GEMINI_LARGE;
    }
    if contains_ascii_case_insensitive(id, "claude-3")
        || contains_ascii_case_insensitive(id, "claude-4")
    {
        return CLAUDE_LARGE;
    }
    if contains_ascii_case_insensitive(id, "gpt-4o")
        || contains_ascii_case_insensitive(id, "gpt-4.1")
        || contains_ascii_case_insensitive(id, "gpt-5")
        || contains_ascii_case_insensitive(id, "o1")
        || contains_ascii_case_insensitive(id, "o3")
        || contains_ascii_case_insensitive(id, "o4")
        || contains_ascii_case_insensitive(id, "deepseek")
        || contains_ascii_case_insensitive(base_url, "deepseek")
        || contains_ascii_case_insensitive(api_type, "deepseek")
    {
        return OPENAI_LARGE;
    }
    DEFAULT
}

pub(crate) fn is_deepseek_v4(model: &str, base_url: &str, api_type: &str) -> bool {
    resolve(model, base_url, api_type).reasoning_wire == ReasoningWire::DeepSeekV4
}

pub(crate) fn model_supports_images(model: &str) -> bool {
    let id = model_id(model);
    MULTIMODAL_MARKERS
        .iter()
        .any(|marker| contains_ascii_case_insensitive(id, marker))
}

const MULTIMODAL_MARKERS: &[&str] = &[
    "vision",
    "-vl",
    "_vl",
    "gemini",
    "claude-3",
    "claude-4",
    "gpt-4o",
    "gpt-4.1",
    "gpt-5",
    "llava",
    "pixtral",
    "qwen2-vl",
    "qwen2.5-vl",
    "qwen3-vl",
    "o1",
    "o3",
    "o4",
];

fn resolve(model: &str, base_url: &str, api_type: &str) -> ResolvedModel {
    let provider = provider_family(model, base_url, api_type);
    let id = model_id(model);

    if is_deepseek_v4_model(model, base_url, provider) {
        let wire = match provider {
            ProviderFamily::OpenRouter => ReasoningWire::OpenRouterReasoning,
            ProviderFamily::Alibaba => ReasoningWire::AlibabaThinkingToggle,
            // A concrete DeepSeek V4 model keeps its native dialect through
            // normal OpenAI-compatible gateways. Only the two providers above
            // publish a documented translation for it.
            _ => ReasoningWire::DeepSeekV4,
        };
        let efforts = match wire {
            ReasoningWire::OpenRouterReasoning => OPENROUTER_DEEPSEEK_V4_EFFORTS,
            ReasoningWire::AlibabaThinkingToggle => ALIBABA_TOGGLE_EFFORTS,
            _ => DEEPSEEK_V4_EFFORTS,
        };
        return ResolvedModel::reasoning(model, efforts, wire, false);
    }

    if is_openai_o_reasoning_model(id) {
        return ResolvedModel::reasoning(
            model,
            OPENAI_O_EFFORTS,
            standard_reasoning_wire(provider),
            true,
        );
    }

    if is_gpt5(id) {
        return ResolvedModel::reasoning(
            model,
            gpt5_reasoning_efforts(id),
            standard_reasoning_wire(provider),
            true,
        );
    }

    if is_glm52(id) {
        let (efforts, wire) = if provider == ProviderFamily::OpenRouter {
            (GLM52_OPENROUTER_EFFORTS, ReasoningWire::OpenRouterReasoning)
        } else {
            (GLM52_COMPATIBLE_EFFORTS, standard_reasoning_wire(provider))
        };
        return ResolvedModel::reasoning(model, efforts, wire, false);
    }

    if contains_ascii_case_insensitive(id, "grok-3-mini") {
        return ResolvedModel::reasoning(
            model,
            GROK3_MINI_EFFORTS,
            standard_reasoning_wire(provider),
            false,
        );
    }

    if contains_ascii_case_insensitive(id, "north-mini-code") {
        return ResolvedModel::reasoning(
            model,
            NORTH_MINI_CODE_EFFORTS,
            standard_reasoning_wire(provider),
            false,
        );
    }

    if is_adjustable_mistral(id) {
        return ResolvedModel::reasoning(
            model,
            MISTRAL_REASONING_EFFORTS,
            standard_reasoning_wire(provider),
            false,
        );
    }

    if provider == ProviderFamily::Alibaba && is_alibaba_toggle_model(id) {
        return ResolvedModel::reasoning(
            model,
            ALIBABA_TOGGLE_EFFORTS,
            ReasoningWire::AlibabaThinkingToggle,
            false,
        );
    }

    if provider == ProviderFamily::OpenRouter && is_openrouter_reasoning_family(id) {
        return ResolvedModel::reasoning(
            model,
            WIDELY_SUPPORTED_EFFORTS,
            ReasoningWire::OpenRouterReasoning,
            false,
        );
    }

    if is_fixed_reasoning_model(id) {
        return ResolvedModel::fixed_reasoning(model);
    }

    ResolvedModel::unknown(model)
}

fn standard_reasoning_wire(provider: ProviderFamily) -> ReasoningWire {
    match provider {
        ProviderFamily::OpenRouter => ReasoningWire::OpenRouterReasoning,
        ProviderFamily::Alibaba => ReasoningWire::AlibabaThinkingToggle,
        // Only explicitly recognized V4 models use DeepSeek's private
        // `thinking` object. Other known model families remain standard
        // OpenAI-compatible requests even when routed through that endpoint.
        ProviderFamily::DeepSeek
        | ProviderFamily::OpenAi
        | ProviderFamily::Copilot
        | ProviderFamily::GenericOpenAiCompatible => ReasoningWire::OpenAiReasoningEffort,
    }
}

fn openrouter_effort(effort: ReasoningEffort) -> &'static str {
    match effort {
        ReasoningEffort::Off => "none",
        _ => effort.as_str(),
    }
}

fn provider_family(model: &str, base_url: &str, api_type: &str) -> ProviderFamily {
    if contains_ascii_case_insensitive(base_url, "openrouter")
        || contains_ascii_case_insensitive(api_type, "openrouter")
        || starts_with_ascii_case_insensitive(model.trim(), "openrouter/")
    {
        return ProviderFamily::OpenRouter;
    }
    if contains_ascii_case_insensitive(base_url, "deepseek")
        || contains_ascii_case_insensitive(api_type, "deepseek")
        || starts_with_ascii_case_insensitive(model.trim(), "deepseek/")
    {
        return ProviderFamily::DeepSeek;
    }
    if contains_ascii_case_insensitive(base_url, "dashscope")
        || contains_ascii_case_insensitive(base_url, "alibaba")
        || contains_ascii_case_insensitive(base_url, "aliyuncs")
        || contains_ascii_case_insensitive(api_type, "alibaba")
        || starts_with_ascii_case_insensitive(model.trim(), "qwen/")
    {
        return ProviderFamily::Alibaba;
    }
    if contains_ascii_case_insensitive(base_url, "githubcopilot")
        || contains_ascii_case_insensitive(base_url, "copilot")
        || contains_ascii_case_insensitive(api_type, "copilot")
    {
        return ProviderFamily::Copilot;
    }
    if contains_ascii_case_insensitive(base_url, "api.openai.com")
        || contains_ascii_case_insensitive(api_type, "openai")
        || starts_with_ascii_case_insensitive(model.trim(), "openai/")
    {
        return ProviderFamily::OpenAi;
    }
    ProviderFamily::GenericOpenAiCompatible
}

fn is_deepseek_v4_model(model: &str, base_url: &str, provider: ProviderFamily) -> bool {
    contains_ascii_case_insensitive(model, "deepseek-v4")
        || contains_ascii_case_insensitive(model, "deepseek/v4")
        || (provider == ProviderFamily::DeepSeek && contains_ascii_case_insensitive(model, "v4"))
        || (contains_ascii_case_insensitive(base_url, "deepseek")
            && contains_ascii_case_insensitive(model, "v4"))
}

fn is_openai_o_reasoning_model(id: &str) -> bool {
    ["o1", "o3", "o4"]
        .iter()
        .any(|prefix| model_matches_prefix(id, prefix))
}

fn is_gpt5(id: &str) -> bool {
    model_matches_prefix(id, "gpt-5")
}

fn gpt5_reasoning_efforts(id: &str) -> &'static [ReasoningEffort] {
    if model_matches_prefix(id, "gpt-5.6-terra") {
        return OPENAI_GPT56_TERRA_EFFORTS;
    }

    let version = gpt5_version(id);
    if contains_ascii_case_insensitive(id, "-chat") {
        return EMPTY_EFFORTS;
    }
    if contains_ascii_case_insensitive(id, "-pro") {
        return if version.is_some_and(|value| value >= 2) {
            OPENAI_GPT5_PRO_2_PLUS_EFFORTS
        } else {
            OPENAI_GPT5_PRO_EFFORTS
        };
    }
    if contains_ascii_case_insensitive(id, "codex") {
        return if version.is_some_and(|value| value >= 3) {
            OPENAI_GPT5_CODEX_3_PLUS_EFFORTS
        } else if contains_ascii_case_insensitive(id, "codex-max")
            || version.is_some_and(|value| value >= 2)
        {
            OPENAI_GPT5_CODEX_XHIGH_EFFORTS
        } else {
            WIDELY_SUPPORTED_EFFORTS
        };
    }
    match version {
        Some(1) => OPENAI_GPT5_1_EFFORTS,
        Some(_) => OPENAI_GPT5_2_PLUS_EFFORTS,
        None => OPENAI_GPT5_BASE_EFFORTS,
    }
}

fn gpt5_version(id: &str) -> Option<u8> {
    let suffix = id.get("gpt-5".len()..)?;
    let digits = suffix
        .strip_prefix('.')
        .or_else(|| suffix.strip_prefix('-'))?;
    let length = digits.bytes().take_while(u8::is_ascii_digit).count();
    if length == 0 {
        return None;
    }
    digits.get(..length)?.parse().ok()
}

fn is_glm52(id: &str) -> bool {
    ["glm-5.2", "glm-5-2", "glm-5p2"]
        .iter()
        .any(|name| contains_ascii_case_insensitive(id, name))
}

fn is_adjustable_mistral(id: &str) -> bool {
    [
        "mistral-small-2603",
        "mistral-small-latest",
        "mistral-medium-3.5",
        "mistral-medium-2604",
    ]
    .iter()
    .any(|name| contains_ascii_case_insensitive(id, name))
}

fn is_alibaba_toggle_model(id: &str) -> bool {
    ["qwen3", "qwq", "kimi-k2", "deepseek-r1"]
        .iter()
        .any(|name| contains_ascii_case_insensitive(id, name))
}

fn is_openrouter_reasoning_family(id: &str) -> bool {
    ["claude", "gemini-2.5", "gemini-3", "grok", "openai/"]
        .iter()
        .any(|name| contains_ascii_case_insensitive(id, name))
}

fn is_fixed_reasoning_model(id: &str) -> bool {
    [
        "deepseek-chat",
        "deepseek-reasoner",
        "deepseek-r1",
        "deepseek-v3",
        "minimax",
        "glm",
        "kimi",
        "k2p",
        "qwen",
        "big-pickle",
        "qwq",
    ]
    .iter()
    .any(|name| contains_ascii_case_insensitive(id, name))
}

fn model_id(model: &str) -> &str {
    let model = model.trim();
    model.rsplit('/').next().unwrap_or(model).trim()
}

fn model_matches_prefix(model: &str, prefix: &str) -> bool {
    let Some(prefix_part) = model.get(..prefix.len()) else {
        return false;
    };
    prefix_part.eq_ignore_ascii_case(prefix)
        && model.get(prefix.len()..).is_some_and(|suffix| {
            matches!(suffix.as_bytes().first(), None | Some(b'-' | b'_' | b'.'))
        })
}

fn starts_with_ascii_case_insensitive(value: &str, prefix: &str) -> bool {
    value
        .get(..prefix.len())
        .is_some_and(|candidate| candidate.eq_ignore_ascii_case(prefix))
}

fn contains_ascii_case_insensitive(value: &str, needle: &str) -> bool {
    value
        .as_bytes()
        .windows(needle.len())
        .any(|candidate| candidate.eq_ignore_ascii_case(needle.as_bytes()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exposes_openai_gpt5_variants_without_matching_gpt50() {
        assert_eq!(
            reasoning_efforts_for_model("gpt-5.2-codex", "https://api.openai.com/v1", "openai"),
            OPENAI_GPT5_CODEX_XHIGH_EFFORTS
        );
        assert!(
            reasoning_efforts_for_model("gpt-50", "https://api.openai.com/v1", "openai").is_empty()
        );
        assert_eq!(
            reasoning_efforts_for_model("gpt-5", "https://api.openai.com/v1", "openai"),
            OPENAI_GPT5_BASE_EFFORTS
        );
        assert!(
            reasoning_efforts_for_model("gpt-5.2-chat", "https://api.openai.com/v1", "openai",)
                .is_empty()
        );
    }

    #[test]
    fn gpt_5_6_terra_matches_codex_reasoning_tiers_and_wire_format() {
        assert_eq!(
            reasoning_efforts_for_model(
                "openai/gpt-5.6-terra",
                "https://api.openai.com/v1",
                "openai"
            ),
            OPENAI_GPT56_TERRA_EFFORTS
        );
        assert!(validate_reasoning_effort(
            "gpt-5.6-terra",
            "https://api.openai.com/v1",
            "openai",
            ReasoningEffort::Max,
        )
        .is_ok());
        assert_eq!(
            ReasoningEffort::parse("ultra"),
            Some(ReasoningEffort::Ultra)
        );

        let mut body = json!({});
        apply_reasoning_effort(
            &mut body,
            "gpt-5.6-terra",
            "https://api.openai.com/v1",
            "openai",
            ReasoningEffort::Max,
        )
        .unwrap();
        assert_eq!(body["reasoning_effort"], "max");
    }

    #[test]
    fn maps_openrouter_variants_to_nested_reasoning_parameter() {
        let mut body = json!({});
        apply_reasoning_effort(
            &mut body,
            "openai/gpt-5.2",
            "https://openrouter.ai/api/v1",
            "openrouter",
            ReasoningEffort::XHigh,
        )
        .unwrap();
        assert_eq!(body["reasoning"]["effort"], "xhigh");
        assert!(body.get("reasoning_effort").is_none());
    }

    #[test]
    fn openrouter_deepseek_v4_uses_its_normalized_effort_set() {
        assert_eq!(
            reasoning_efforts_for_model(
                "deepseek/deepseek-v4-flash",
                "https://openrouter.ai/api/v1",
                "openrouter",
            ),
            OPENROUTER_DEEPSEEK_V4_EFFORTS
        );

        let mut body = json!({});
        apply_reasoning_effort(
            &mut body,
            "deepseek/deepseek-v4-flash",
            "https://openrouter.ai/api/v1",
            "openrouter",
            ReasoningEffort::XHigh,
        )
        .unwrap();
        assert_eq!(body["reasoning"]["effort"], "xhigh");
        assert!(validate_reasoning_effort(
            "deepseek/deepseek-v4-flash",
            "https://openrouter.ai/api/v1",
            "openrouter",
            ReasoningEffort::Off,
        )
        .is_err());
        assert_eq!(openrouter_effort(ReasoningEffort::Off), "none");
    }

    #[test]
    fn deepseek_endpoint_only_uses_v4_dialect_for_v4_models() {
        let mut body = json!({});
        apply_reasoning_effort(
            &mut body,
            "o3-mini",
            "https://api.deepseek.com/v1",
            "",
            ReasoningEffort::High,
        )
        .unwrap();

        assert_eq!(body["reasoning_effort"], "high");
        assert!(body.get("thinking").is_none());
    }

    #[test]
    fn deepseek_v4_keeps_its_native_thinking_dialect() {
        let mut disabled = json!({});
        apply_reasoning_effort(
            &mut disabled,
            "deepseek-v4-flash",
            "https://api.deepseek.com/v1",
            "",
            ReasoningEffort::Off,
        )
        .unwrap();
        assert_eq!(disabled["thinking"]["type"], "disabled");

        let mut maximum = json!({});
        apply_reasoning_effort(
            &mut maximum,
            "deepseek-v4-flash",
            "https://api.deepseek.com/v1",
            "",
            ReasoningEffort::Max,
        )
        .unwrap();
        assert_eq!(maximum["thinking"]["type"], "enabled");
        assert_eq!(maximum["reasoning_effort"], "max");
    }

    #[test]
    fn fixed_reasoning_models_are_not_mistaken_for_adjustable_models() {
        let model = capabilities("deepseek-reasoner", "https://api.deepseek.com/v1", "");
        assert!(model.capabilities.reasoning);
        assert!(model.variants.is_empty());
        assert!(validate_reasoning_effort(
            "deepseek-reasoner",
            "https://api.deepseek.com/v1",
            "",
            ReasoningEffort::High,
        )
        .is_err());
    }
}
