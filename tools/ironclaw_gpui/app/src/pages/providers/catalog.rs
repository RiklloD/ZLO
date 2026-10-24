//! Presentation data for the Providers page: names and kinds for the
//! gateway's provider ids, and the CLI agents IronClaw can drive. The
//! gateway stays the source of truth for which providers exist.

/// How a provider connects.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// A subscription sign-in or an existing account login.
    Account,
    ApiKey,
    /// A server on this Mac; no key.
    Local,
}

impl Kind {
    pub fn label(self) -> &'static str {
        match self {
            Kind::Account => "Sign in",
            Kind::ApiKey => "API key",
            Kind::Local => "Local",
        }
    }
}

pub fn kind_of(id: &str) -> Kind {
    match id {
        "openai_codex" | "gemini_oauth" | "github_copilot" | "nearai" => Kind::Account,
        "ollama" | "lmstudio" | "openai_compatible" => Kind::Local,
        _ => Kind::ApiKey,
    }
}

/// The name people know a provider by; unknown ids are title-cased.
pub fn display_name(id: &str) -> String {
    let known = match id {
        "openai_codex" => "ChatGPT plan",
        "gemini_oauth" => "Gemini (Google login)",
        "github_copilot" => "GitHub Copilot",
        "nearai" => "NEAR AI",
        "openai" => "OpenAI API",
        "anthropic" => "Anthropic API",
        "openai_compatible" => "OpenAI-compatible server",
        "openrouter" => "OpenRouter",
        "deepseek" => "DeepSeek",
        "zai" => "Z.AI (GLM)",
        "sambanova" => "SambaNova",
        "ionet" => "io.net",
        "minimax" => "MiniMax",
        "xai" => "xAI (Grok)",
        "huggingface" => "Hugging Face",
        "moonshot" => "Kimi / Moonshot",
        "dashscope" => "Qwen (DashScope)",
        "deepinfra" => "DeepInfra",
        "nebius" => "Nebius Token Factory",
        "vercel_ai_gateway" => "Vercel AI Gateway",
        "ollama_cloud" => "Ollama Cloud",
        "stepfun" => "StepFun",
        "upstage" => "Upstage Solar",
        "inception" => "Inception (Mercury)",
        "gmi" => "GMI Cloud",
        "kilocode" => "Kilo Code Gateway",
        "opencode_zen" => "OpenCode Zen",
        "xiaomi" => "Xiaomi MiMo",
        "meta" => "Meta Llama API",
        "lmstudio" => "LM Studio",
        "bedrock" => "AWS Bedrock",
        _ => "",
    };
    if !known.is_empty() {
        return known.to_string();
    }
    id.split(['_', '-'])
        .filter(|w| !w.is_empty())
        .map(|w| {
            let mut c = w.chars();
            c.next()
                .map(|f| f.to_uppercase().collect::<String>() + c.as_str())
                .unwrap_or_default()
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// A coding-agent CLI the orchestrator can run threads on.
pub struct KnownCli {
    /// Runtime id prefix the gateway registers (`claude-cli`, `claude-cli-2`).
    pub runtime: &'static str,
    pub name: &'static str,
    pub blurb: &'static str,
    /// Shell command that installs it, when there is a public one.
    pub install: Option<&'static str>,
    /// Shell command that signs it in.
    pub login: Option<&'static str>,
    /// IronClaw has no driver yet.
    pub planned: bool,
}

pub const KNOWN_CLIS: &[KnownCli] = &[
    KnownCli {
        runtime: "claude-cli",
        name: "Claude Code",
        blurb: "Anthropic's agent on your Claude Pro or Max plan.",
        install: Some("npm install -g @anthropic-ai/claude-code"),
        login: Some("claude /login"),
        planned: false,
    },
    KnownCli {
        runtime: "antigravity",
        name: "Antigravity",
        blurb: "Google's agent over ACP, on your Google account.",
        install: None,
        login: None,
        planned: false,
    },
    KnownCli {
        runtime: "muse",
        name: "Muse Code",
        blurb: "Meta's coding agent.",
        install: None,
        login: None,
        planned: false,
    },
    KnownCli {
        runtime: "codex",
        name: "Codex CLI",
        blurb: "OpenAI's agent on your ChatGPT plan, in its own harness.",
        install: Some("npm install -g @openai/codex"),
        login: Some("codex login"),
        planned: false,
    },
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_and_kinds() {
        assert_eq!(display_name("openai_codex"), "ChatGPT plan");
        assert_eq!(display_name("some_new-host"), "Some New Host");
        assert!(kind_of("openai_codex") == Kind::Account);
        assert!(kind_of("lmstudio") == Kind::Local);
        assert!(kind_of("groq") == Kind::ApiKey);
    }
}
