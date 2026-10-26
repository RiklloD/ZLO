//! The app's asset source: gpui-kit's default component icons plus the few
//! extra Lucide icons the inbox and goal strip use (a few KB, instead of
//! embedding the whole 1,830-icon catalog).

use std::borrow::Cow;

use gpui_kit::assets::{Assets, icon_assets};
use gpui_kit::{AssetSource, SharedString};

icon_assets!(
    ExtraIcons,
    [
        Pin,
        PinOff,
        AlarmClock,
        GitBranch,
        GitFork,
        ListFilter,
        Target,
        Pencil,
        Bell,
        SquarePen,
        Puzzle,
        Zap,
        SlidersHorizontal,
        Sparkles,
        LayoutGrid,
        Monitor,
        Paperclip,
        // Right pane tabs and actions.
        Activity,
        FileDiff,
        FileCode,
        FolderGit,
        FolderPlus,
        GitCommitHorizontal,
        GitCompare,
        GitPullRequest,
        Image,
        ListTodo,
        MessageSquarePlus,
        CircleDot,
        Upload
    ]
);

/// Provider logos for the model picker and Providers page: t3code's
/// (MIT) and lobe-icons' monochrome set (MIT). See THIRD_PARTY_NOTICES.md.
const PROVIDER_ICONS: &[(&str, &[u8])] = &[
    (
        "providers/antigravity.png",
        include_bytes!("../assets/providers/antigravity.png"),
    ),
    (
        "providers/arcee.svg",
        include_bytes!("../assets/providers/arcee.svg"),
    ),
    (
        "providers/aws.svg",
        include_bytes!("../assets/providers/aws.svg"),
    ),
    (
        "providers/cerebras.svg",
        include_bytes!("../assets/providers/cerebras.svg"),
    ),
    (
        "providers/claude.svg",
        include_bytes!("../assets/providers/claude.svg"),
    ),
    (
        "providers/cloudflare.svg",
        include_bytes!("../assets/providers/cloudflare.svg"),
    ),
    (
        "providers/codex.svg",
        include_bytes!("../assets/providers/codex.svg"),
    ),
    (
        "providers/cursor.svg",
        include_bytes!("../assets/providers/cursor.svg"),
    ),
    (
        "providers/deepinfra.svg",
        include_bytes!("../assets/providers/deepinfra.svg"),
    ),
    (
        "providers/deepseek.svg",
        include_bytes!("../assets/providers/deepseek.svg"),
    ),
    (
        "providers/fireworks.svg",
        include_bytes!("../assets/providers/fireworks.svg"),
    ),
    (
        "providers/gemini.svg",
        include_bytes!("../assets/providers/gemini.svg"),
    ),
    (
        "providers/githubcopilot.svg",
        include_bytes!("../assets/providers/githubcopilot.svg"),
    ),
    (
        "providers/grok.svg",
        include_bytes!("../assets/providers/grok.svg"),
    ),
    (
        "providers/groq.svg",
        include_bytes!("../assets/providers/groq.svg"),
    ),
    (
        "providers/huggingface.svg",
        include_bytes!("../assets/providers/huggingface.svg"),
    ),
    (
        "providers/inception.svg",
        include_bytes!("../assets/providers/inception.svg"),
    ),
    (
        "providers/kilocode.svg",
        include_bytes!("../assets/providers/kilocode.svg"),
    ),
    (
        "providers/kimi.svg",
        include_bytes!("../assets/providers/kimi.svg"),
    ),
    (
        "providers/lmstudio.svg",
        include_bytes!("../assets/providers/lmstudio.svg"),
    ),
    (
        "providers/meta.svg",
        include_bytes!("../assets/providers/meta.svg"),
    ),
    (
        "providers/minimax.svg",
        include_bytes!("../assets/providers/minimax.svg"),
    ),
    (
        "providers/mistral.svg",
        include_bytes!("../assets/providers/mistral.svg"),
    ),
    (
        "providers/muse.svg",
        include_bytes!("../assets/providers/muse.svg"),
    ),
    (
        "providers/nebius.svg",
        include_bytes!("../assets/providers/nebius.svg"),
    ),
    (
        "providers/novita.svg",
        include_bytes!("../assets/providers/novita.svg"),
    ),
    (
        "providers/nvidia.svg",
        include_bytes!("../assets/providers/nvidia.svg"),
    ),
    (
        "providers/ollama.svg",
        include_bytes!("../assets/providers/ollama.svg"),
    ),
    (
        "providers/openai.svg",
        include_bytes!("../assets/providers/openai.svg"),
    ),
    (
        "providers/opencode.svg",
        include_bytes!("../assets/providers/opencode.svg"),
    ),
    (
        "providers/openrouter.svg",
        include_bytes!("../assets/providers/openrouter.svg"),
    ),
    (
        "providers/pi.svg",
        include_bytes!("../assets/providers/pi.svg"),
    ),
    (
        "providers/qwen.svg",
        include_bytes!("../assets/providers/qwen.svg"),
    ),
    (
        "providers/sambanova.svg",
        include_bytes!("../assets/providers/sambanova.svg"),
    ),
    (
        "providers/stepfun.svg",
        include_bytes!("../assets/providers/stepfun.svg"),
    ),
    (
        "providers/together.svg",
        include_bytes!("../assets/providers/together.svg"),
    ),
    (
        "providers/upstage.svg",
        include_bytes!("../assets/providers/upstage.svg"),
    ),
    (
        "providers/venice.svg",
        include_bytes!("../assets/providers/venice.svg"),
    ),
    (
        "providers/vercel.svg",
        include_bytes!("../assets/providers/vercel.svg"),
    ),
    (
        "providers/xai.svg",
        include_bytes!("../assets/providers/xai.svg"),
    ),
    (
        "providers/yandex.svg",
        include_bytes!("../assets/providers/yandex.svg"),
    ),
    (
        "providers/zai.svg",
        include_bytes!("../assets/providers/zai.svg"),
    ),
];

/// Lucide ships at stroke 2; T3 Code's design pass found 1.75 reads lighter
/// beside text. Rewritten once per icon at load (gpui caches the result).
fn thin_stroke(path: &str, bytes: Cow<'static, [u8]>) -> Cow<'static, [u8]> {
    const FROM: &[u8] = br#"stroke-width="2""#;
    if !path.starts_with("icons/") || !bytes.windows(FROM.len()).any(|w| w == FROM) {
        return bytes;
    }
    match std::str::from_utf8(&bytes) {
        Ok(svg) => Cow::Owned(
            svg.replace(r#"stroke-width="2""#, r#"stroke-width="1.75""#)
                .into_bytes(),
        ),
        Err(_) => bytes,
    }
}

pub struct AppAssets;

impl AssetSource for AppAssets {
    fn load(&self, path: &str) -> gpui_kit::Result<Option<Cow<'static, [u8]>>> {
        if let Some((_, bytes)) = PROVIDER_ICONS.iter().find(|(p, _)| *p == path) {
            return Ok(Some(Cow::Borrowed(bytes)));
        }
        let bytes = match ExtraIcons.load(path)? {
            Some(bytes) => Some(bytes),
            None => Assets.load(path)?,
        };
        Ok(bytes.map(|b| thin_stroke(path, b)))
    }

    fn list(&self, path: &str) -> gpui_kit::Result<Vec<SharedString>> {
        let mut paths = Assets.list(path)?;
        paths.extend(ExtraIcons.list(path)?);
        paths.sort();
        paths.dedup();
        Ok(paths)
    }
}

/// The extra icons by name, so screens never spell asset paths.
pub use gpui_kit::assets::IconName as ExtraIcon;
