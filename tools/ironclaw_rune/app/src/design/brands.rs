//! Provider logos (GPUI `design/brands.rs` and the provider half of
//! `assets.rs`): which bundled icon (`assets/providers/`, t3code's and
//! lobe-icons' sets, MIT) and tint stand for a provider, runtime or LLM
//! backend id. Unknown ids get a lettered tile, so a new provider never
//! renders blank.
//!
//! GPUI drew the SVGs through its asset source with `text_color` tinting
//! monochrome ones. Rune rasterizes SVGs through `rune::ImageCache` (resvg on
//! its worker) at the size they draw at; a monochrome logo is recoloured by
//! rewriting its `fill="black"` / `currentColor` before it is cached, keyed
//! by colour, so a theme switch gets a new raster once and then reuses it.
//!
//! Public API:
//! - [`provider_logo`]`(s, ids, size) -> El`
//! - [`preload`]`(s, ids, size)` decodes now (headless fixtures)
//! - [`Logos`] lives in `LookState::logos`; `look::start` gives it its cache.

use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::Arc;

use rune::{Color, El, Image, ImageCache, SurfaceFit, el, text};

use crate::state::State;

/// The device scale logos rasterize at; mips cover smaller draws.
const RASTER_SCALE: f32 = 2.;

/// A bundled logo: file name and, for monochrome SVGs, a brand tint
/// (`None` uses the text colour).
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

macro_rules! bundled {
    ($($name:literal),* $(,)?) => {
        /// Every logo's bytes by file name.
        const FILES: &[(&str, &[u8])] = &[$(
            ($name, include_bytes!(concat!("../../assets/providers/", $name))),
        )*];
    };
}

bundled![
    "antigravity.png",
    "arcee.svg",
    "aws.svg",
    "cerebras.svg",
    "claude.svg",
    "cloudflare.svg",
    "codex.svg",
    "cursor.svg",
    "deepinfra.svg",
    "deepseek.svg",
    "fireworks.svg",
    "gemini.svg",
    "githubcopilot.svg",
    "grok.svg",
    "groq.svg",
    "huggingface.svg",
    "inception.svg",
    "kilocode.svg",
    "kimi.svg",
    "lmstudio.svg",
    "meta.svg",
    "minimax.svg",
    "mistral.svg",
    "muse.svg",
    "nebius.svg",
    "novita.svg",
    "nvidia.svg",
    "ollama.svg",
    "openai.svg",
    "opencode.svg",
    "openrouter.svg",
    "pi.svg",
    "qwen.svg",
    "sambanova.svg",
    "stepfun.svg",
    "together.svg",
    "upstage.svg",
    "venice.svg",
    "vercel.svg",
    "xai.svg",
    "yandex.svg",
    "zai.svg",
];

/// Exact ids first (LLM provider ids from `providers.json`, CLI runtime ids),
/// then families matched by substring for runtime/provider pairs.
fn logo_for(key: &str) -> Option<Logo> {
    let key = key.trim().to_ascii_lowercase();
    let exact = match key.as_str() {
        "openai" | "openai_codex" | "codex" | "chatgpt" => Some(mono("openai.svg")),
        "anthropic" => Some(tinted("claude.svg", 0xD97757)),
        "gemini" | "gemini_oauth" => Some(tinted("gemini.svg", 0x3186FF)),
        "openrouter" => Some(mono("openrouter.svg")),
        "mistral" => Some(tinted("mistral.svg", 0xFA520F)),
        "deepseek" => Some(tinted("deepseek.svg", 0x4D6BFE)),
        "groq" => Some(tinted("groq.svg", 0xF55036)),
        "ollama" | "ollama_cloud" => Some(mono("ollama.svg")),
        "xai" => Some(mono("xai.svg")),
        "huggingface" => Some(tinted("huggingface.svg", 0xFFD21E)),
        "moonshot" | "kimi" => Some(mono("kimi.svg")),
        "dashscope" | "qwen" => Some(tinted("qwen.svg", 0x615CED)),
        "nvidia" => Some(tinted("nvidia.svg", 0x76B900)),
        "together" => Some(mono("together.svg")),
        "fireworks" => Some(tinted("fireworks.svg", 0x8B5CF6)),
        "cerebras" => Some(tinted("cerebras.svg", 0xF15A29)),
        "zai" | "glm" | "zhipu" => Some(mono("zai.svg")),
        "minimax" => Some(tinted("minimax.svg", 0xF23F5D)),
        "bedrock" => Some(tinted("aws.svg", 0xFF9900)),
        "github_copilot" => Some(mono("githubcopilot.svg")),
        "cloudflare" => Some(tinted("cloudflare.svg", 0xF38020)),
        "venice" => Some(mono("venice.svg")),
        "sambanova" => Some(tinted("sambanova.svg", 0xEE7624)),
        "yandex" => Some(tinted("yandex.svg", 0xFC3F1D)),
        "deepinfra" => Some(mono("deepinfra.svg")),
        "nebius" => Some(mono("nebius.svg")),
        "novita" => Some(mono("novita.svg")),
        "vercel_ai_gateway" => Some(mono("vercel.svg")),
        "stepfun" => Some(mono("stepfun.svg")),
        "arcee" => Some(mono("arcee.svg")),
        "upstage" => Some(tinted("upstage.svg", 0x805CFB)),
        "inception" => Some(mono("inception.svg")),
        "kilocode" => Some(mono("kilocode.svg")),
        "meta" => Some(tinted("meta.svg", 0x0081FB)),
        "lmstudio" => Some(mono("lmstudio.svg")),
        "opencode" | "opencode_zen" => Some(mono("opencode.svg")),
        "cursor" => Some(mono("cursor.svg")),
        "grok" => Some(mono("grok.svg")),
        "pi" => Some(mono("pi.svg")),
        _ => None,
    };
    if exact.is_some() {
        return exact;
    }
    // Runtime ids carry an account suffix (`claude-cli-2`, `muse-code`).
    if key.contains("claude") {
        Some(tinted("claude.svg", 0xD97757))
    } else if key.contains("antigravity") {
        Some(Logo {
            file: "antigravity.png",
            tint: None,
            raster: true,
        })
    } else if key.contains("muse") {
        Some(tinted("muse.svg", 0x0081FB))
    } else if key.contains("codex") || key.contains("openai") {
        Some(mono("openai.svg"))
    } else {
        None
    }
}

/// Decoded logos, kept alive here (the cache only shares live handles).
#[derive(Default)]
pub struct Logos {
    cache: Option<ImageCache>,
    /// (file, colour hex, size in points × 4) → image.
    images: RefCell<HashMap<(&'static str, String, u32), Image>>,
}

impl Logos {
    /// Called once by `look::start` with a waker that redraws on decode.
    pub fn start(&mut self, waker: rune::Waker) {
        self.cache = Some(ImageCache::new(waker));
    }

    /// The logo's image at `size` points in `color` (ignored for rasters).
    fn image(&self, logo: &Logo, color: Color, size: f32) -> Option<Image> {
        let cache = self.cache.as_ref()?;
        let hex = if logo.raster { String::new() } else { color.hex() };
        let key = (logo.file, hex, (size * 4.).round() as u32);
        if let Some(image) = self.images.borrow().get(&key) {
            return Some(image.clone());
        }
        let bytes = FILES.iter().find(|(f, _)| *f == logo.file)?.1;
        let image = if logo.raster {
            cache.load(Arc::<[u8]>::from(bytes))
        } else {
            cache.svg(recolor(bytes, &key.1), size, RASTER_SCALE)
        };
        let mut images = self.images.borrow_mut();
        if images.len() > 512 {
            images.clear();
        }
        images.insert(key, image.clone());
        Some(image)
    }
}

/// A monochrome SVG painted in `hex` (`#rrggbb`): the root `fill="black"`
/// and any `currentColor` take it.
fn recolor(bytes: &[u8], hex: &str) -> Arc<[u8]> {
    match std::str::from_utf8(bytes) {
        Ok(svg) => {
            let hex6 = hex.get(..7).unwrap_or(hex);
            let out = svg
                .replace("currentColor", hex6)
                .replace(r#"fill="black""#, &format!(r#"fill="{hex6}""#))
                .replace(r##"fill="#000""##, &format!(r#"fill="{hex6}""#))
                .replace(r##"fill="#000000""##, &format!(r#"fill="{hex6}""#));
            Arc::from(out.into_bytes())
        }
        Err(_) => Arc::from(bytes),
    }
}

/// The logo for the first id that has one, else a lettered tile from the
/// first id.
pub fn provider_logo(s: &State, ids: &[&str], size: f32) -> El {
    let p = &s.palette;
    let found = ids.iter().find_map(|id| logo_for(id));
    let image = found.as_ref().and_then(|logo| {
        let color = logo.tint.map(super::rgb).unwrap_or(p.text);
        s.look.logos.image(logo, color, size)
    });
    match image {
        Some(image) if image.error().is_none() => el()
            .size(size, size)
            .a11y_hidden()
            .pixels_fit(image.pixels(), SurfaceFit::Fit),
        _ => letter_tile(s, ids.first().copied().unwrap_or("?"), size),
    }
}

/// Decode `ids`' logos now, for a headless fixture that snaps one frame.
pub fn preload(s: &State, ids: &[&str], size: f32) {
    for id in ids {
        if let Some(logo) = logo_for(id) {
            let color = logo.tint.map(super::rgb).unwrap_or(s.palette.text);
            if let Some(image) = s.look.logos.image(&logo, color, size)
                && let Err(e) = image.wait()
            {
                tracing::warn!("provider logo {} failed to decode: {e:?}", logo.file);
            }
        }
    }
}

fn letter_tile(s: &State, id: &str, size: f32) -> El {
    let p = &s.palette;
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
                .font(rune::tok::TextStyle::new((size * 0.55).round(), 500.))
                .color(p.accent),
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_and_runtime_families_resolve() {
        assert_eq!(logo_for("openai_codex").map(|l| l.file), Some("openai.svg"));
        assert_eq!(logo_for("claude-cli-2").map(|l| l.file), Some("claude.svg"));
        assert_eq!(logo_for("muse-code").map(|l| l.file), Some("muse.svg"));
        assert!(logo_for("antigravity").is_some_and(|l| l.raster));
        assert!(logo_for("nearai").is_none());
        // "pi" is exact only: it must not catch every id containing "pi".
        assert!(logo_for("api_gateway").is_none());
    }

    #[test]
    fn every_logo_is_bundled() {
        for id in [
            "openai", "anthropic", "gemini", "openrouter", "mistral", "deepseek", "groq",
            "ollama", "xai", "huggingface", "moonshot", "dashscope", "nvidia", "together",
            "fireworks", "cerebras", "zai", "minimax", "bedrock", "github_copilot", "cloudflare",
            "venice", "sambanova", "yandex", "deepinfra", "nebius", "novita",
            "vercel_ai_gateway", "stepfun", "arcee", "upstage", "inception", "kilocode", "meta",
            "lmstudio", "opencode_zen", "cursor", "grok", "pi", "antigravity", "muse",
        ] {
            let logo = logo_for(id).unwrap();
            assert!(FILES.iter().any(|(f, _)| *f == logo.file), "{id}");
        }
    }

    #[test]
    fn monochrome_logos_take_the_tint() {
        let out = recolor(br#"<svg fill="black"><path fill="currentColor"/></svg>"#, "#ff8a4c");
        let out = std::str::from_utf8(&out).unwrap();
        assert!(!out.contains("black") && !out.contains("currentColor"));
        assert_eq!(out.matches("#ff8a4c").count(), 2);
    }
}
