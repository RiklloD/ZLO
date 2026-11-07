//! Fixture data for the Providers scenes (`--scene providers`,
//! `providers-catalog`, `providers-cli`).

use ironclaw_gpui_client::operator_llm::{
    LlmActiveSelection, LlmConfigSnapshot, LlmProviderView, UserModelCatalog,
};
use ironclaw_gpui_client::orchestration::{PickerPrefs, RouteProvider};
use ironclaw_gpui_client::task_models::{TaskModelChoice, TaskModels};

use super::{Models, Tab};
use crate::state::{Conn, Page, State};

fn provider(id: &str, adapter: &str, description: &str, key_required: bool) -> LlmProviderView {
    LlmProviderView {
        id: id.into(),
        description: description.into(),
        adapter: adapter.into(),
        default_model: "default".into(),
        base_url: None,
        builtin: true,
        active: false,
        active_model: None,
        api_key_required: key_required,
        accepts_api_key: key_required,
        api_key_set: false,
        can_list_models: true,
    }
}

fn base(s: &mut State) {
    s.fixture = true;
    s.conn = Conn::Ready;
    s.page = Page::Providers;
    let mut anthropic = provider(
        "anthropic",
        "anthropic",
        "Claude models on your Anthropic API key",
        true,
    );
    anthropic.active = true;
    anthropic.active_model = Some("claude-opus-5-5".into());
    anthropic.api_key_set = true;
    let mut zai = provider("zai", "open_ai_completions", "GLM models from Z.AI", true);
    zai.api_key_set = true;
    let providers = vec![
        anthropic,
        zai,
        provider(
            "openai_codex",
            "openai_codex",
            "Use your ChatGPT Plus or Pro plan",
            false,
        ),
        provider(
            "github_copilot",
            "github_copilot",
            "Copilot models on your GitHub login",
            false,
        ),
        provider(
            "openai",
            "open_ai_completions",
            "GPT models on your OpenAI API key",
            true,
        ),
        provider(
            "openrouter",
            "open_ai_completions",
            "Hundreds of models behind one key",
            true,
        ),
        provider(
            "mistral",
            "open_ai_completions",
            "Mistral and Codestral",
            true,
        ),
        provider(
            "groq",
            "open_ai_completions",
            "Fast inference on LPUs",
            true,
        ),
        provider(
            "deepseek",
            "open_ai_completions",
            "DeepSeek V4 and R2",
            true,
        ),
        provider(
            "some_new-host",
            "open_ai_completions",
            "A provider with no logo yet",
            true,
        ),
        provider(
            "ollama",
            "open_ai_completions",
            "Models served by Ollama on this Mac",
            false,
        ),
        provider(
            "lmstudio",
            "open_ai_completions",
            "Models served by LM Studio",
            false,
        ),
    ];
    let p = &mut s.pages.providers;
    p.llm = Some(LlmConfigSnapshot {
        providers,
        active: Some(LlmActiveSelection {
            provider_id: "anthropic".into(),
            model: Some("claude-opus-5-5".into()),
        }),
        user_model_policy: None,
    });
    p.catalog = Some(UserModelCatalog {
        selection_enabled: true,
        workspace_default: Some("claude-opus-5-5".into()),
        models: vec!["claude-opus-5-5".into(), "claude-sonnet-5-5".into()],
        model_entries: Vec::new(),
    });
    p.task_models = Some(TaskModels {
        thread_titles: Some(TaskModelChoice {
            provider_id: "anthropic".into(),
            model: "claude-haiku-5-5".into(),
        }),
    });
    p.catalog_thread = Some("t1".into());
    p.picker = Some(PickerPrefs::default());
    p.runtimes = Some(vec![
        RouteProvider {
            provider: "claude-cli".into(),
            name: "Claude Code".into(),
            runtime: "claude-cli".into(),
            authenticated: true,
            models: vec!["opus".into(), "sonnet".into(), "haiku".into()],
            ..Default::default()
        },
        RouteProvider {
            provider: "codex".into(),
            name: "Codex CLI".into(),
            runtime: "codex".into(),
            authenticated: false,
            ..Default::default()
        },
        RouteProvider {
            provider: "ironclaw".into(),
            name: "IronClaw".into(),
            runtime: "ironclaw".into(),
            authenticated: true,
            models: vec!["claude-opus-5-5".into(), "claude-haiku-5-5".into()],
            ..Default::default()
        },
    ]);
}

/// Connected providers with the models list open and a connection result.
pub fn providers(s: &mut State) {
    base(s);
    let p = &mut s.pages.providers;
    p.notice = Some("Now using anthropic / claude-opus-5-5".into());
    p.models.insert(
        "anthropic".into(),
        Models::Loaded(vec![
            "claude-opus-5-5".into(),
            "claude-sonnet-5-5".into(),
            "claude-haiku-5-5".into(),
        ]),
    );
    p.probes.insert(
        "zai".into(),
        Some((true, "Reached the provider and listed 12 models.".into())),
    );
    p.titles_open = true;
}

/// "Add a provider" with a tile open on its API-key field.
pub fn catalog(s: &mut State) {
    base(s);
    let p = &mut s.pages.providers;
    p.adding = Some("openrouter".into());
    p.key_editor = Some("openrouter".into());
    p.confirm_delete = Some("zai".into());
}

/// The CLI agents tab, one agent removed.
pub fn cli(s: &mut State) {
    base(s);
    let p = &mut s.pages.providers;
    p.tab = Tab::Cli;
    if let Some(picker) = p.picker.as_mut() {
        picker.toggle_provider("codex");
    }
}
