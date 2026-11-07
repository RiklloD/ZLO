//! Provider logos: which bundled image (`assets/providers/`) and tint stand
//! for a provider, runtime or LLM backend id; unknown ids get a lettered
//! tile, so a new provider never renders blank. The mapping is GPUI
//! `design/brands.rs`, kept local while area G ports it.
//!
//! PORT-STUB(area G): use design::brands (provider_logo) once it lands.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

use rune::{Color, Cx, El, Image, ImageCache, TextStyle, el, text};

use crate::design::{self, Palette};
use crate::state::State;

/// A bundled logo: file stem and, for monochrome SVGs, a brand tint
/// (`None` uses the text colour).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Logo {
    file: &'static str,
    tint: Option<u32>,
    raster: bool,
}

const fn mono(file: &'static str) -> Logo {
    Logo {
        file,
        tint: None,
        raster: false,
    }
}

const fn tinted(file: &'static str, tint: u32) -> Logo {
    Logo {
        file,
        tint: Some(tint),
        raster: false,
    }
}

/// Exact ids first (LLM provider ids, CLI runtime ids), then families by
/// substring for runtime/provider pairs.
fn logo_for(key: &str) -> Option<Logo> {
    let key = key.trim().to_ascii_lowercase();
    let exact = match key.as_str() {
        "openai" | "openai_codex" | "codex" | "chatgpt" => Some(mono("openai")),
        "anthropic" => Some(tinted("claude", 0xD97757)),
        "gemini" | "gemini_oauth" => Some(tinted("gemini", 0x3186FF)),
        "openrouter" => Some(mono("openrouter")),
        "mistral" => Some(tinted("mistral", 0xFA520F)),
        "deepseek" => Some(tinted("deepseek", 0x4D6BFE)),
        "groq" => Some(tinted("groq", 0xF55036)),
        "ollama" | "ollama_cloud" => Some(mono("ollama")),
        "xai" => Some(mono("xai")),
        "huggingface" => Some(tinted("huggingface", 0xFFD21E)),
        "moonshot" | "kimi" => Some(mono("kimi")),
        "dashscope" | "qwen" => Some(tinted("qwen", 0x615CED)),
        "nvidia" => Some(tinted("nvidia", 0x76B900)),
        "together" => Some(mono("together")),
        "fireworks" => Some(tinted("fireworks", 0x8B5CF6)),
        "cerebras" => Some(tinted("cerebras", 0xF15A29)),
        "zai" | "glm" | "zhipu" => Some(mono("zai")),
        "minimax" => Some(tinted("minimax", 0xF23F5D)),
        "bedrock" => Some(tinted("aws", 0xFF9900)),
        "github_copilot" => Some(mono("githubcopilot")),
        "cloudflare" => Some(tinted("cloudflare", 0xF38020)),
        "venice" => Some(mono("venice")),
        "sambanova" => Some(tinted("sambanova", 0xEE7624)),
        "yandex" => Some(tinted("yandex", 0xFC3F1D)),
        "deepinfra" => Some(mono("deepinfra")),
        "nebius" => Some(mono("nebius")),
        "novita" => Some(mono("novita")),
        "vercel_ai_gateway" => Some(mono("vercel")),
        "stepfun" => Some(mono("stepfun")),
        "arcee" => Some(mono("arcee")),
        "upstage" => Some(tinted("upstage", 0x805CFB)),
        "inception" => Some(mono("inception")),
        "kilocode" => Some(mono("kilocode")),
        "meta" => Some(tinted("meta", 0x0081FB)),
        "lmstudio" => Some(mono("lmstudio")),
        "opencode" | "opencode_zen" => Some(mono("opencode")),
        "cursor" => Some(mono("cursor")),
        "grok" => Some(mono("grok")),
        "pi" => Some(mono("pi")),
        _ => None,
    };
    if exact.is_some() {
        return exact;
    }
    // Runtime ids carry an account suffix (`claude-cli-2`, `muse-code`).
    if key.contains("claude") {
        Some(tinted("claude", 0xD97757))
    } else if key.contains("antigravity") {
        Some(Logo {
            file: "antigravity",
            tint: None,
            raster: true,
        })
    } else if key.contains("muse") {
        Some(tinted("muse", 0x0081FB))
    } else if key.contains("codex") || key.contains("openai") {
        Some(mono("openai"))
    } else {
        None
    }
}

fn bytes(file: &str) -> Option<&'static [u8]> {
    Some(match file {
        "arcee" => include_bytes!("../../../assets/providers/arcee.svg"),
        "aws" => include_bytes!("../../../assets/providers/aws.svg"),
        "cerebras" => include_bytes!("../../../assets/providers/cerebras.svg"),
        "claude" => include_bytes!("../../../assets/providers/claude.svg"),
        "cloudflare" => include_bytes!("../../../assets/providers/cloudflare.svg"),
        "codex" => include_bytes!("../../../assets/providers/codex.svg"),
        "cursor" => include_bytes!("../../../assets/providers/cursor.svg"),
        "deepinfra" => include_bytes!("../../../assets/providers/deepinfra.svg"),
        "deepseek" => include_bytes!("../../../assets/providers/deepseek.svg"),
        "fireworks" => include_bytes!("../../../assets/providers/fireworks.svg"),
        "gemini" => include_bytes!("../../../assets/providers/gemini.svg"),
        "githubcopilot" => include_bytes!("../../../assets/providers/githubcopilot.svg"),
        "grok" => include_bytes!("../../../assets/providers/grok.svg"),
        "groq" => include_bytes!("../../../assets/providers/groq.svg"),
        "huggingface" => include_bytes!("../../../assets/providers/huggingface.svg"),
        "inception" => include_bytes!("../../../assets/providers/inception.svg"),
        "kilocode" => include_bytes!("../../../assets/providers/kilocode.svg"),
        "kimi" => include_bytes!("../../../assets/providers/kimi.svg"),
        "lmstudio" => include_bytes!("../../../assets/providers/lmstudio.svg"),
        "meta" => include_bytes!("../../../assets/providers/meta.svg"),
        "minimax" => include_bytes!("../../../assets/providers/minimax.svg"),
        "mistral" => include_bytes!("../../../assets/providers/mistral.svg"),
        "muse" => include_bytes!("../../../assets/providers/muse.svg"),
        "nebius" => include_bytes!("../../../assets/providers/nebius.svg"),
        "novita" => include_bytes!("../../../assets/providers/novita.svg"),
        "nvidia" => include_bytes!("../../../assets/providers/nvidia.svg"),
        "ollama" => include_bytes!("../../../assets/providers/ollama.svg"),
        "openai" => include_bytes!("../../../assets/providers/openai.svg"),
        "opencode" => include_bytes!("../../../assets/providers/opencode.svg"),
        "openrouter" => include_bytes!("../../../assets/providers/openrouter.svg"),
        "pi" => include_bytes!("../../../assets/providers/pi.svg"),
        "qwen" => include_bytes!("../../../assets/providers/qwen.svg"),
        "sambanova" => include_bytes!("../../../assets/providers/sambanova.svg"),
        "stepfun" => include_bytes!("../../../assets/providers/stepfun.svg"),
        "together" => include_bytes!("../../../assets/providers/together.svg"),
        "upstage" => include_bytes!("../../../assets/providers/upstage.svg"),
        "venice" => include_bytes!("../../../assets/providers/venice.svg"),
        "vercel" => include_bytes!("../../../assets/providers/vercel.svg"),
        "xai" => include_bytes!("../../../assets/providers/xai.svg"),
        "yandex" => include_bytes!("../../../assets/providers/yandex.svg"),
        "zai" => include_bytes!("../../../assets/providers/zai.svg"),
        "antigravity" => include_bytes!("../../../assets/providers/antigravity.png"),
        _ => return None,
    })
}

/// A monochrome SVG drawn in one colour: black fills and `currentColor`
/// take `color`, as GPUI's `svg().text_color(..)` paints it.
fn tint_svg(svg: &[u8], color: Color) -> Vec<u8> {
    let src = String::from_utf8_lossy(svg);
    // `#rrggbbaa`; SVG takes the opaque part.
    let hex: String = color.hex().chars().take(7).collect();
    let out = src
        .replace("fill=\"black\"", "fill=\"currentColor\"")
        .replace("fill=\"#000\"", "fill=\"currentColor\"")
        .replace("fill=\"#000000\"", "fill=\"currentColor\"")
        .replacen("<svg ", &format!("<svg color=\"{hex}\" "), 1);
    out.into_bytes()
}

static CACHE: OnceLock<ImageCache> = OnceLock::new();
type Key = (&'static str, u32, String);
static IMAGES: OnceLock<Mutex<HashMap<Key, Image>>> = OnceLock::new();

fn cache(s: &State) -> &'static ImageCache {
    CACHE.get_or_init(|| {
        let handle = s.rt.handle.clone();
        ImageCache::new(Arc::new(move || handle.wake()))
    })
}

fn image(s: &State, logo: Logo, size: f32, scale: f32, color: Color) -> Option<Image> {
    let raw = bytes(logo.file)?;
    let px = (size * scale).round() as u32;
    let key = (logo.file, px, color.hex());
    let images = IMAGES.get_or_init(Default::default);
    let mut map = images.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(img) = map.get(&key) {
        return Some(img.clone());
    }
    let cache = cache(s);
    let img = if logo.raster {
        cache.load(raw.to_vec())
    } else {
        cache.svg(tint_svg(raw, color), size, scale)
    };
    // Headless scenes draw one frame: decode before it.
    if s.fixture
        && let Err(e) = img.wait()
    {
        tracing::warn!(logo = logo.file, error = %e, "provider logo failed to decode");
    }
    map.insert(key, img.clone());
    Some(img)
}

/// The logo for the first id that has one, else a lettered tile from the
/// first id.
pub fn provider_logo(s: &State, cx: &Cx<State>, ids: &[&str], size: f32) -> El {
    let p = &s.palette;
    let Some(logo) = ids.iter().find_map(|id| logo_for(id)) else {
        return letter_tile(p, ids.first().copied().unwrap_or("?"), size);
    };
    let color = logo.tint.map(design::rgb).unwrap_or(p.text);
    match image(s, logo, size, cx.env().scale, color) {
        Some(img) => el()
            .size(size, size)
            .pixels_fit(img.pixels(), rune::SurfaceFit::Fit)
            .a11y_hidden(),
        None => letter_tile(p, ids.first().copied().unwrap_or("?"), size),
    }
}

fn letter_tile(p: &Palette, id: &str, size: f32) -> El {
    let letter = id
        .chars()
        .find(|c| c.is_alphanumeric())
        .map(|c| c.to_ascii_uppercase().to_string())
        .unwrap_or_else(|| "?".into());
    el().size(size, size)
        .radius(size / 2.)
        .border(1., p.accent.alpha(0.5))
        .fill(p.accent.alpha(0.12))
        .center()
        .child(
            text(letter)
                .font(TextStyle::new(size * 0.55, 500.))
                .color(p.accent),
        )
}

#[cfg(test)]
mod tests {
    use super::logo_for;

    #[test]
    fn ids_and_runtime_families_resolve() {
        assert_eq!(logo_for("openai_codex").map(|l| l.file), Some("openai"));
        assert_eq!(logo_for("claude-cli-2").map(|l| l.file), Some("claude"));
        assert!(logo_for("antigravity-1").is_some_and(|l| l.raster));
        assert_eq!(logo_for("unknown_host"), None);
    }

    #[test]
    fn every_logo_has_bytes() {
        for id in [
            "openai",
            "anthropic",
            "gemini",
            "bedrock",
            "muse-code",
            "antigravity",
        ] {
            let logo = logo_for(id).map(|l| l.file);
            assert!(logo.and_then(super::bytes).is_some(), "{id}");
        }
    }
}
