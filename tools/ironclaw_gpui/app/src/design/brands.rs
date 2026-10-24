//! Provider logos: which bundled icon (`assets/providers/`) and tint stand
//! for a provider, runtime or LLM backend id. Unknown ids get a lettered
//! tile, so a new provider never renders blank.

use gpui_kit::{App, Hsla, IntoElement, ParentElement, Styled, div, img, px, rgb, svg};

use super::Palette;

/// A bundled logo: asset path and, for monochrome SVGs, a brand tint
/// (`None` uses the text color).
struct Logo {
    path: &'static str,
    tint: Option<u32>,
    raster: bool,
}

const fn mono(path: &'static str) -> Logo {
    Logo {
        path,
        tint: None,
        raster: false,
    }
}

const fn tinted(path: &'static str, tint: u32) -> Logo {
    Logo {
        path,
        tint: Some(tint),
        raster: false,
    }
}

/// Exact ids first (LLM provider ids from `providers.json`, CLI runtime ids),
/// then families matched by substring for runtime/provider pairs.
fn logo_for(key: &str) -> Option<Logo> {
    let key = key.trim().to_ascii_lowercase();
    let exact = match key.as_str() {
        "openai" | "openai_codex" | "codex" | "chatgpt" => Some(mono("providers/openai.svg")),
        "anthropic" => Some(tinted("providers/claude.svg", 0xD97757)),
        "gemini" | "gemini_oauth" => Some(tinted("providers/gemini.svg", 0x3186FF)),
        "openrouter" => Some(mono("providers/openrouter.svg")),
        "mistral" => Some(tinted("providers/mistral.svg", 0xFA520F)),
        "deepseek" => Some(tinted("providers/deepseek.svg", 0x4D6BFE)),
        "groq" => Some(tinted("providers/groq.svg", 0xF55036)),
        "ollama" | "ollama_cloud" => Some(mono("providers/ollama.svg")),
        "xai" => Some(mono("providers/xai.svg")),
        "huggingface" => Some(tinted("providers/huggingface.svg", 0xFFD21E)),
        "moonshot" | "kimi" => Some(mono("providers/kimi.svg")),
        "dashscope" | "qwen" => Some(tinted("providers/qwen.svg", 0x615CED)),
        "nvidia" => Some(tinted("providers/nvidia.svg", 0x76B900)),
        "together" => Some(mono("providers/together.svg")),
        "fireworks" => Some(tinted("providers/fireworks.svg", 0x8B5CF6)),
        "cerebras" => Some(tinted("providers/cerebras.svg", 0xF15A29)),
        "zai" | "glm" | "zhipu" => Some(mono("providers/zai.svg")),
        "minimax" => Some(tinted("providers/minimax.svg", 0xF23F5D)),
        "bedrock" => Some(tinted("providers/aws.svg", 0xFF9900)),
        "github_copilot" => Some(mono("providers/githubcopilot.svg")),
        "cloudflare" => Some(tinted("providers/cloudflare.svg", 0xF38020)),
        "venice" => Some(mono("providers/venice.svg")),
        "sambanova" => Some(tinted("providers/sambanova.svg", 0xEE7624)),
        "yandex" => Some(tinted("providers/yandex.svg", 0xFC3F1D)),
        "deepinfra" => Some(mono("providers/deepinfra.svg")),
        "nebius" => Some(mono("providers/nebius.svg")),
        "novita" => Some(mono("providers/novita.svg")),
        "vercel_ai_gateway" => Some(mono("providers/vercel.svg")),
        "stepfun" => Some(mono("providers/stepfun.svg")),
        "arcee" => Some(mono("providers/arcee.svg")),
        "upstage" => Some(tinted("providers/upstage.svg", 0x805CFB)),
        "inception" => Some(mono("providers/inception.svg")),
        "kilocode" => Some(mono("providers/kilocode.svg")),
        "meta" => Some(tinted("providers/meta.svg", 0x0081FB)),
        "lmstudio" => Some(mono("providers/lmstudio.svg")),
        "opencode" | "opencode_zen" => Some(mono("providers/opencode.svg")),
        "cursor" => Some(mono("providers/cursor.svg")),
        "grok" => Some(mono("providers/grok.svg")),
        "pi" => Some(mono("providers/pi.svg")),
        _ => None,
    };
    if exact.is_some() {
        return exact;
    }
    // Runtime ids carry an account suffix (`claude-cli-2`, `muse-code`).
    if key.contains("claude") {
        Some(tinted("providers/claude.svg", 0xD97757))
    } else if key.contains("antigravity") {
        Some(Logo {
            path: "providers/antigravity.png",
            tint: None,
            raster: true,
        })
    } else if key.contains("muse") {
        Some(tinted("providers/muse.svg", 0x0081FB))
    } else if key.contains("codex") || key.contains("openai") {
        Some(mono("providers/openai.svg"))
    } else {
        None
    }
}

/// The logo for the first id that has one, else a lettered tile from the
/// first id.
pub fn provider_logo(ids: &[&str], size: f32, cx: &App) -> gpui_kit::AnyElement {
    let p = *Palette::of(cx);
    match ids.iter().find_map(|id| logo_for(id)) {
        Some(logo) if logo.raster => img(logo.path).flex_none().size(px(size)).into_any_element(),
        Some(logo) => {
            let color: Hsla = logo.tint.map(|t| rgb(t).into()).unwrap_or(p.text);
            svg()
                .path(logo.path)
                .flex_none()
                .size(px(size))
                .text_color(color)
                .into_any_element()
        }
        None => letter_tile(ids.first().copied().unwrap_or("?"), size, cx).into_any_element(),
    }
}

fn letter_tile(id: &str, size: f32, cx: &App) -> impl IntoElement {
    let p = Palette::of(cx);
    let letter = id
        .chars()
        .find(|c| c.is_alphanumeric())
        .map(|c| c.to_ascii_uppercase().to_string())
        .unwrap_or_else(|| "?".into());
    div()
        .flex_none()
        .size(px(size))
        .rounded_full()
        .border_1()
        .border_color(p.accent.opacity(0.5))
        .bg(p.accent.opacity(0.12))
        .text_color(p.accent)
        .text_size(px(size * 0.55))
        .flex()
        .items_center()
        .justify_center()
        .child(letter)
}

#[cfg(test)]
mod tests {
    use super::logo_for;

    #[test]
    fn ids_and_runtime_families_resolve() {
        assert_eq!(
            logo_for("openai_codex").map(|l| l.path),
            Some("providers/openai.svg")
        );
        assert_eq!(
            logo_for("claude-cli-2").map(|l| l.path),
            Some("providers/claude.svg")
        );
        assert_eq!(
            logo_for("muse-code").map(|l| l.path),
            Some("providers/muse.svg")
        );
        assert!(logo_for("antigravity").is_some_and(|l| l.raster));
        assert!(logo_for("nearai").is_none());
        // "pi" is exact only: it must not catch every id containing "pi".
        assert!(logo_for("api_gateway").is_none());
    }

    #[test]
    fn every_logo_is_bundled() {
        let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/assets/");
        for id in [
            "openai",
            "anthropic",
            "gemini",
            "openrouter",
            "mistral",
            "deepseek",
            "groq",
            "ollama",
            "xai",
            "huggingface",
            "moonshot",
            "dashscope",
            "nvidia",
            "together",
            "fireworks",
            "cerebras",
            "zai",
            "minimax",
            "bedrock",
            "github_copilot",
            "cloudflare",
            "venice",
            "sambanova",
            "yandex",
            "deepinfra",
            "nebius",
            "novita",
            "vercel_ai_gateway",
            "stepfun",
            "arcee",
            "upstage",
            "inception",
            "kilocode",
            "meta",
            "lmstudio",
            "opencode_zen",
            "cursor",
            "grok",
            "pi",
            "antigravity",
            "muse",
        ] {
            let logo = logo_for(id).unwrap();
            assert!(
                std::path::Path::new(&format!("{dir}{}", logo.path)).is_file(),
                "{id}"
            );
        }
    }
}
