//! Bedrock Converse prompt caching: explicit `cachePoint` blocks.
//!
//! Mirrors the Anthropic transports' breakpoint layout (#6984): one cache
//! point after the system prompt, one after the last tool definition, and one
//! at the end of the last message, so the tool/system prefix and the growing
//! conversation cache independently. Three of Bedrock's four allowed
//! checkpoints; all share one TTL (`5m` default, `1h` under `Long`).
//!
//! Only models Bedrock documents as cache-capable get cache points — a
//! `cachePoint` sent to any other model is a validation error, not a no-op.

use aws_sdk_bedrockruntime::types::{
    CachePointBlock, CachePointType, CacheTtl, ContentBlock, Message, SystemContentBlock, Tool,
    ToolConfiguration,
};

use crate::config::CacheRetention;
use crate::error::LlmError;

/// Whether Bedrock supports Converse prompt caching for `model_id` (with or
/// without a cross-region prefix such as `us.` / `global.`).
///
/// Claude: 3.5 Haiku, 3.7 Sonnet, and every Claude 4+ family (denylist of
/// the documented-unsupported older families, so a new Claude family caches
/// without an update). Amazon Nova: micro / lite / pro / premier.
pub(super) fn supports_prompt_cache(model_id: &str) -> bool {
    let lower = model_id.to_ascii_lowercase();
    let model = ["us.", "eu.", "apac.", "global."]
        .iter()
        .find_map(|prefix| lower.strip_prefix(prefix))
        .unwrap_or(&lower);
    if let Some(claude) = model.strip_prefix("anthropic.claude-") {
        let unsupported = [
            "v2",
            "instant",
            "3-haiku",
            "3-sonnet",
            "3-opus",
            "3-5-sonnet",
        ];
        return !unsupported.iter().any(|family| claude.starts_with(family));
    }
    [
        "amazon.nova-micro",
        "amazon.nova-lite",
        "amazon.nova-pro",
        "amazon.nova-premier",
    ]
    .iter()
    .any(|family| model.starts_with(family))
}

/// The cache point every breakpoint in a request carries, or `None` when
/// caching is off or the model cannot cache.
pub(super) fn cache_point(
    retention: CacheRetention,
    model_id: &str,
) -> Result<Option<CachePointBlock>, LlmError> {
    let ttl = match retention {
        CacheRetention::None => return Ok(None),
        _ if !supports_prompt_cache(model_id) => return Ok(None),
        CacheRetention::Short => None,
        CacheRetention::Long => Some(CacheTtl::OneHour),
    };
    CachePointBlock::builder()
        .r#type(CachePointType::Default)
        .set_ttl(ttl)
        .build()
        .map(Some)
        .map_err(|e| LlmError::RequestFailed {
            provider: "bedrock".to_string(),
            reason: format!("Failed to build CachePointBlock: {e}"),
        })
}

/// Append the system-prompt and last-message cache points. A request with no
/// system prompt gets no system point; an empty transcript gets none at all.
pub(super) fn apply_message_cache_points(
    point: &CachePointBlock,
    system: &mut Vec<SystemContentBlock>,
    messages: &mut Vec<Message>,
) -> Result<(), LlmError> {
    if !system.is_empty() {
        system.push(SystemContentBlock::CachePoint(point.clone()));
    }
    if let Some(role) = messages.last().map(|message| message.role().clone()) {
        // `push_message` merges into the trailing message of the same role,
        // so this appends the point as that message's last content block.
        super::push_message(
            messages,
            role,
            vec![ContentBlock::CachePoint(point.clone())],
        )?;
    }
    Ok(())
}

/// Rebuild `config` with the tool-prefix cache point after the last tool
/// definition, keeping its tool choice.
pub(super) fn with_tool_cache_point(
    config: ToolConfiguration,
    point: &CachePointBlock,
) -> Result<ToolConfiguration, LlmError> {
    let mut tools = config.tools().to_vec();
    tools.push(Tool::CachePoint(point.clone()));
    ToolConfiguration::builder()
        .set_tools(Some(tools))
        .set_tool_choice(config.tool_choice().cloned())
        .build()
        .map_err(|e| LlmError::RequestFailed {
            provider: "bedrock".to_string(),
            reason: format!("Failed to build cached ToolConfiguration: {e}"),
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::{ChatMessage, ToolCall, ToolDefinition};

    #[test]
    fn cache_support_follows_bedrock_model_families() {
        for model in [
            "anthropic.claude-opus-4-6-v1",
            "us.anthropic.claude-sonnet-4-5-20250929-v1:0",
            "global.anthropic.claude-haiku-4-5-20251001-v1:0",
            "anthropic.claude-3-7-sonnet-20250219-v1:0",
            "anthropic.claude-3-5-haiku-20241022-v1:0",
            "anthropic.claude-fable-5-1-v1",
            "amazon.nova-pro-v1:0",
            "eu.amazon.nova-lite-v1:0",
        ] {
            assert!(supports_prompt_cache(model), "{model}");
        }
        for model in [
            "anthropic.claude-v2:1",
            "anthropic.claude-instant-v1",
            "anthropic.claude-3-haiku-20240307-v1:0",
            "anthropic.claude-3-5-sonnet-20241022-v2:0",
            "meta.llama3-70b-instruct-v1:0",
            "mistral.mistral-large-2407-v1:0",
        ] {
            assert!(!supports_prompt_cache(model), "{model}");
        }
    }

    #[test]
    fn cache_point_carries_the_retention_ttl() {
        let model = "anthropic.claude-opus-4-6-v1";
        assert!(cache_point(CacheRetention::None, model).unwrap().is_none());
        let short = cache_point(CacheRetention::Short, model).unwrap().unwrap();
        assert_eq!(short.r#type(), &CachePointType::Default);
        assert!(short.ttl().is_none(), "short uses Bedrock's 5m default");
        let long = cache_point(CacheRetention::Long, model).unwrap().unwrap();
        assert_eq!(long.ttl(), Some(&CacheTtl::OneHour));
        assert!(
            cache_point(CacheRetention::Long, "meta.llama3-70b-instruct-v1:0")
                .unwrap()
                .is_none(),
            "a non-caching model must never receive a cachePoint"
        );
    }

    /// The breakpoint layout over a real converted agent-loop transcript:
    /// system tail, last-tool tail, and last-message tail (here the tool
    /// result user turn), with no extra message created.
    #[test]
    fn cache_points_land_after_system_tools_and_last_message() {
        let messages = vec![
            ChatMessage::system("You are helpful."),
            ChatMessage::user("Run the tool."),
            ChatMessage::assistant_with_tool_calls(
                None,
                vec![ToolCall {
                    id: "call_1".to_string(),
                    name: "alpha".to_string(),
                    arguments: serde_json::json!({}),
                    ..ToolCall::default()
                }],
            ),
            ChatMessage::tool_result("call_1", "alpha", "tool says hi"),
        ];
        let (mut system, mut converted) = super::super::convert_messages(&messages).unwrap();
        let count = converted.len();
        let point = cache_point(CacheRetention::Short, "anthropic.claude-opus-4-6-v1")
            .unwrap()
            .unwrap();
        apply_message_cache_points(&point, &mut system, &mut converted).unwrap();

        assert!(matches!(
            system.last(),
            Some(SystemContentBlock::CachePoint(_))
        ));
        assert_eq!(converted.len(), count, "the point joins the last message");
        let last = converted.last().unwrap().content();
        assert!(matches!(last.last(), Some(ContentBlock::CachePoint(_))));
        assert!(matches!(last.first(), Some(ContentBlock::ToolResult(_))));
        let points = converted
            .iter()
            .flat_map(|message| message.content())
            .filter(|block| matches!(block, ContentBlock::CachePoint(_)))
            .count();
        assert_eq!(points, 1);

        let tools = super::super::build_tool_config(
            &[ToolDefinition {
                name: "alpha".to_string(),
                description: "First".to_string(),
                parameters: serde_json::json!({"type":"object","properties":{}}),
            }],
            Some("required"),
        )
        .unwrap()
        .unwrap();
        let cached = with_tool_cache_point(tools, &point).unwrap();
        assert_eq!(cached.tools().len(), 2);
        assert!(matches!(cached.tools().last(), Some(Tool::CachePoint(_))));
        assert!(cached.tool_choice().is_some_and(|choice| choice.is_any()));
    }

    #[test]
    fn no_system_prompt_gets_no_system_point() {
        let (mut system, mut converted) =
            super::super::convert_messages(&[ChatMessage::user("hi")]).unwrap();
        let point = cache_point(CacheRetention::Short, "amazon.nova-pro-v1:0")
            .unwrap()
            .unwrap();
        apply_message_cache_points(&point, &mut system, &mut converted).unwrap();
        assert!(system.is_empty());
        assert!(matches!(
            converted[0].content().last(),
            Some(ContentBlock::CachePoint(_))
        ));
    }
}
