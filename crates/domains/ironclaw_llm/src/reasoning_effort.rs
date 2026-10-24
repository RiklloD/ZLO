//! Per-request reasoning effort for API providers.
//!
//! The model router puts a turn's effort in the request metadata
//! ([`crate::provider_routing::REASONING_EFFORT_METADATA`]). Each protocol
//! spells it differently on the wire; [`EffortStyle`] maps it, and
//! [`efforts_for`] says which levels a model takes, so the model picker only
//! offers those. A level a model does not take is dropped, never sent.

use serde_json::{Value, json};

use crate::registry::ProviderProtocol;

const OPENAI_LEVELS: &[&str] = &["low", "medium", "high"];
/// The Responses API levels the ChatGPT plan models take.
const RESPONSES_LEVELS: &[&str] = &["low", "medium", "high", "xhigh"];
const ANTHROPIC_LEVELS: &[&str] = &["low", "medium", "high"];
const ANTHROPIC_OPUS_LEVELS: &[&str] = &["low", "medium", "high", "max"];
const GEMINI_LEVEL_LEVELS: &[&str] = &["low", "high"];
const GEMINI_BUDGET_LEVELS: &[&str] = &["low", "medium", "high"];
const GROK_MINI_LEVELS: &[&str] = &["low", "high"];
/// GLM has a thinking switch, no depth.
const GLM_LEVELS: &[&str] = &["off", "on"];

/// How a rig-backed protocol carries the effort.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EffortStyle {
    /// Chat Completions: top-level `reasoning_effort`; GLM's `thinking`.
    OpenAiChat,
    /// Messages API: adaptive `thinking` plus `output_config.effort`.
    Anthropic,
    /// `generationConfig.thinkingConfig` (level on 3+, budget on 2.5).
    Gemini,
    /// OpenRouter's normalized `reasoning.effort`.
    OpenRouter,
}

/// What else the request carries; some combinations reject thinking.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct RequestShape {
    pub temperature: bool,
    pub forced_tool: bool,
    pub structured_output: bool,
}

/// The levels `model` on `protocol` (provider `provider_id`) takes; empty:
/// no effort knob.
pub fn efforts_for(protocol: ProviderProtocol, provider_id: &str, model: &str) -> Vec<String> {
    let model = model.to_ascii_lowercase();
    let levels: &[&str] = match protocol {
        ProviderProtocol::OpenAiCodex if openai_reasoning(&model) => RESPONSES_LEVELS,
        ProviderProtocol::OpenAiCompletions => openai_chat_levels(provider_id, &model),
        ProviderProtocol::Anthropic => anthropic_levels(&model),
        ProviderProtocol::Gemini => gemini_levels(&model),
        ProviderProtocol::OpenRouter if openrouter_reasons(&model) => OPENAI_LEVELS,
        _ => &[],
    };
    levels.iter().map(|level| level.to_string()).collect()
}

/// The extra request params for `effort` on `model`, or `None` when the
/// model has no such level or the request shape rules thinking out.
pub(crate) fn params(
    style: EffortStyle,
    provider_id: &str,
    model: &str,
    effort: &str,
    shape: RequestShape,
) -> Option<Value> {
    let model = model.to_ascii_lowercase();
    match style {
        EffortStyle::OpenAiChat => {
            let levels = openai_chat_levels(provider_id, &model);
            if !levels.contains(&effort) {
                return None;
            }
            if levels == GLM_LEVELS {
                let kind = if effort == "on" {
                    "enabled"
                } else {
                    "disabled"
                };
                return Some(json!({ "thinking": { "type": kind } }));
            }
            Some(json!({ "reasoning_effort": effort }))
        }
        EffortStyle::Anthropic => {
            if !anthropic_levels(&model).contains(&effort) || shape.structured_output {
                return None;
            }
            let mut params = json!({ "output_config": { "effort": effort } });
            // Thinking rejects a set temperature and a forced tool.
            if !shape.temperature && !shape.forced_tool {
                params["thinking"] = json!({ "type": "adaptive" });
            }
            Some(params)
        }
        EffortStyle::Gemini => {
            let levels = gemini_levels(&model);
            if !levels.contains(&effort) {
                return None;
            }
            let thinking = if levels == GEMINI_LEVEL_LEVELS {
                json!({ "thinkingLevel": effort })
            } else {
                let budget = match effort {
                    "low" => 1024,
                    "medium" => 8192,
                    _ => 24576,
                };
                json!({ "thinkingBudget": budget })
            };
            Some(json!({ "generationConfig": { "thinkingConfig": thinking } }))
        }
        EffortStyle::OpenRouter => (openrouter_reasons(&model) && OPENAI_LEVELS.contains(&effort))
            .then(|| json!({ "reasoning": { "effort": effort } })),
    }
}

fn openai_chat_levels(provider_id: &str, model: &str) -> &'static [&'static str] {
    let tail = model.rsplit('/').next().unwrap_or(model);
    if tail.starts_with("glm-") && version_after(tail, "glm-") >= Some((4, 5)) {
        return GLM_LEVELS;
    }
    if tail.starts_with("grok-3-mini") {
        return GROK_MINI_LEVELS;
    }
    // xAI's other models reason on their own and reject the field.
    if provider_id == "xai" || tail.starts_with("grok") {
        return &[];
    }
    if openai_reasoning(tail) || tail.contains("gpt-oss") {
        return OPENAI_LEVELS;
    }
    &[]
}

/// OpenAI reasoning models: o-series and GPT-5 onwards.
pub(crate) fn openai_reasoning(model: &str) -> bool {
    let tail = model.rsplit('/').next().unwrap_or(model);
    let o_series = tail.len() > 1
        && tail.starts_with('o')
        && tail[1..].starts_with(|c: char| c.is_ascii_digit());
    o_series
        || version_after(tail, "gpt-").is_some_and(|(major, _)| major >= 5)
        || tail.contains("codex")
}

/// Claude 4.6 and later think adaptively and take an effort.
fn anthropic_levels(model: &str) -> &'static [&'static str] {
    let tail = model.rsplit('/').next().unwrap_or(model);
    if !tail.starts_with("claude-") {
        return &[];
    }
    // `claude-opus-4-6`, `claude-fable-5-1`: the first number after the family.
    let version = tail
        .split('-')
        .skip(1)
        .skip_while(|part| !part.starts_with(|c: char| c.is_ascii_digit()))
        .take(2)
        .map(|part| part.parse::<u32>().ok())
        .collect::<Vec<_>>();
    let (major, minor) = match version.as_slice() {
        [Some(major), Some(minor)] if *minor < 100 => (*major, *minor),
        [Some(major), ..] => (*major, 0),
        _ => return &[],
    };
    if (major, minor) < (4, 6) {
        return &[];
    }
    if tail.contains("opus") {
        ANTHROPIC_OPUS_LEVELS
    } else {
        ANTHROPIC_LEVELS
    }
}

fn gemini_levels(model: &str) -> &'static [&'static str] {
    let tail = model.rsplit('/').next().unwrap_or(model);
    match version_after(tail, "gemini-") {
        Some((major, _)) if major >= 3 => GEMINI_LEVEL_LEVELS,
        Some((2, 5)) => GEMINI_BUDGET_LEVELS,
        _ => &[],
    }
}

fn openrouter_reasons(model: &str) -> bool {
    openai_reasoning(model)
        || !anthropic_levels(model).is_empty()
        || !gemini_levels(model).is_empty()
        || crate::reasoning_models::has_native_thinking(model)
}

/// `(major, minor)` of the version right after `prefix`: `gpt-6.1-sol` ->
/// `(6, 1)`, `glm-4.5-air` -> `(4, 5)`, `gemini-3-pro` -> `(3, 0)`.
fn version_after(model: &str, prefix: &str) -> Option<(u32, u32)> {
    let rest = model.strip_prefix(prefix)?;
    let version: String = rest
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == '.')
        .collect();
    let mut parts = version.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next().and_then(|m| m.parse().ok()).unwrap_or(0);
    Some((major, minor))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn levels(protocol: ProviderProtocol, provider: &str, model: &str) -> Vec<String> {
        efforts_for(protocol, provider, model)
    }

    #[test]
    fn only_models_with_a_knob_offer_levels() {
        use ProviderProtocol::*;
        assert_eq!(levels(OpenAiCodex, "openai_codex", "gpt-6.1-sol").len(), 4);
        assert!(levels(OpenAiCodex, "openai_codex", "gpt-4o").is_empty());
        assert_eq!(levels(OpenAiCompletions, "openai", "gpt-5-mini").len(), 3);
        assert_eq!(levels(OpenAiCompletions, "openai", "o3").len(), 3);
        assert!(levels(OpenAiCompletions, "openai", "gpt-4o").is_empty());
        assert_eq!(levels(OpenAiCompletions, "glm", "glm-5.3"), ["off", "on"]);
        assert!(levels(OpenAiCompletions, "glm", "glm-4").is_empty());
        assert_eq!(
            levels(OpenAiCompletions, "xai", "grok-3-mini"),
            ["low", "high"]
        );
        assert!(levels(OpenAiCompletions, "xai", "grok-4").is_empty());
        assert!(levels(OpenAiCompletions, "inception", "mercury-2").is_empty());
        assert_eq!(levels(Anthropic, "anthropic", "claude-opus-5-5").len(), 4);
        assert_eq!(levels(Anthropic, "anthropic", "claude-sonnet-4-6").len(), 3);
        assert!(levels(Anthropic, "anthropic", "claude-sonnet-4-5-20250929").is_empty());
        assert!(levels(Anthropic, "anthropic", "claude-3-7-sonnet-latest").is_empty());
        assert!(levels(Anthropic, "anthropic", "claude-sonnet-4-20250514").is_empty());
        assert_eq!(levels(Gemini, "gemini", "gemini-4-argon"), ["low", "high"]);
        assert_eq!(levels(Gemini, "gemini", "gemini-2.5-flash").len(), 3);
        assert!(levels(Gemini, "gemini", "gemini-2.0-flash").is_empty());
        assert_eq!(
            levels(OpenRouter, "openrouter", "anthropic/claude-opus-4-7").len(),
            3
        );
        assert!(levels(Ollama, "ollama", "qwen3").is_empty());
    }

    #[test]
    fn each_protocol_spells_the_effort_its_way() {
        let shape = RequestShape::default();
        assert_eq!(
            params(EffortStyle::OpenAiChat, "openai", "gpt-5", "high", shape),
            Some(json!({ "reasoning_effort": "high" }))
        );
        assert_eq!(
            params(EffortStyle::OpenAiChat, "glm", "glm-5.3", "off", shape),
            Some(json!({ "thinking": { "type": "disabled" } }))
        );
        assert_eq!(
            params(
                EffortStyle::Anthropic,
                "anthropic",
                "claude-opus-5-5",
                "max",
                shape
            ),
            Some(
                json!({ "output_config": { "effort": "max" }, "thinking": { "type": "adaptive" } })
            )
        );
        assert_eq!(
            params(
                EffortStyle::Gemini,
                "gemini",
                "gemini-2.5-pro",
                "medium",
                shape
            ),
            Some(json!({ "generationConfig": { "thinkingConfig": { "thinkingBudget": 8192 } } }))
        );
        assert_eq!(
            params(
                EffortStyle::OpenRouter,
                "openrouter",
                "openai/o4-mini",
                "low",
                shape
            ),
            Some(json!({ "reasoning": { "effort": "low" } }))
        );
    }

    #[test]
    fn unsupported_levels_and_shapes_send_nothing() {
        let shape = RequestShape::default();
        assert_eq!(
            params(EffortStyle::OpenAiChat, "openai", "gpt-4o", "high", shape),
            None
        );
        assert_eq!(
            params(EffortStyle::OpenAiChat, "openai", "gpt-5", "xhigh", shape),
            None
        );
        assert_eq!(
            params(
                EffortStyle::Gemini,
                "gemini",
                "gemini-4-argon",
                "medium",
                shape
            ),
            None
        );
        let structured = RequestShape {
            structured_output: true,
            ..shape
        };
        assert_eq!(
            params(
                EffortStyle::Anthropic,
                "anthropic",
                "claude-opus-5-5",
                "low",
                structured
            ),
            None
        );
        let forced = RequestShape {
            forced_tool: true,
            ..shape
        };
        assert_eq!(
            params(
                EffortStyle::Anthropic,
                "anthropic",
                "claude-opus-5-5",
                "low",
                forced
            ),
            Some(json!({ "output_config": { "effort": "low" } })),
            "a forced tool keeps the effort but not thinking"
        );
    }
}
