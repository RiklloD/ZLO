//! Presentation data for the Providers page: names and kinds for the gateway's provider ids, and the CLI agents IronClaw can
//! drive. The gateway stays the source of truth for which providers exist.

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

/// A coding-agent CLI the orchestrator can run threads on. The one list
/// of them: Settings → Providers → CLI agents and setup's "Connect your
/// agents" both read it.
pub struct KnownCli {
    /// Runtime id prefix the gateway registers (`claude-cli`, `claude-cli-2`).
    pub runtime: &'static str,
    pub name: &'static str,
    pub blurb: &'static str,
    /// The program its installer puts on the login shell's PATH.
    pub binary: &'static str,
    /// Shell command that installs it, when there is a public one. The
    /// native installers (t3code `NATIVE_INSTALL_COMMANDS`): no Node or npm
    /// needed.
    pub install: Option<&'static str>,
    /// Shell command that signs it in.
    pub login: Option<&'static str>,
    /// IronClaw has no driver yet.
    pub planned: bool,
    /// The gateway can find it through a legacy runtime with no sign-in
    /// command of its own (Antigravity's ACP runtime), not only its CLI.
    pub legacy_runtime: bool,
}

pub const KNOWN_CLIS: &[KnownCli] = &[
    KnownCli {
        runtime: "claude-cli",
        name: "Claude Code",
        blurb: "Anthropic's agent on your Claude Pro or Max plan.",
        binary: "claude",
        install: Some("curl -fsSL https://claude.ai/install.sh | bash"),
        login: Some("claude auth login"),
        planned: false,
        legacy_runtime: false,
    },
    KnownCli {
        runtime: "antigravity",
        name: "Antigravity",
        blurb: "Google's agent over ACP, on your Google account.",
        binary: "agy",
        // antigravity.google/docs/getting-started (macOS and Linux).
        install: Some("curl -fsSL https://antigravity.google/cli/install.sh | bash"),
        // Signs in with Google in the browser, then exits (`agy acp
        // --help`: "ACP terminal auth"). agy 1.5 keeps the sign-in in the OS
        // keyring, which `agy acp` shares whatever profile it runs on.
        login: Some("agy acp --login"),
        planned: false,
        legacy_runtime: true,
    },
    KnownCli {
        runtime: "muse",
        name: "Muse Code",
        blurb: "Meta's coding agent.",
        binary: "muse",
        install: None,
        login: None,
        planned: false,
        legacy_runtime: false,
    },
    KnownCli {
        runtime: "codex",
        name: "Codex CLI",
        blurb: "OpenAI's agent on your ChatGPT plan, in its own harness.",
        binary: "codex",
        install: Some("curl -fsSL https://chatgpt.com/codex/install.sh | sh"),
        login: Some("codex login"),
        planned: false,
        legacy_runtime: false,
    },
];

/// What a found CLI row offers next, as (label, shell command) pairs:
/// nothing once signed in; installing the CLI when the runtime found has
/// no sign-in command of its own (Antigravity's legacy ACP runtime); both
/// its sign-in and installing the CLI when the gateway did not say which
/// it found (an older gateway) and it can be the legacy one; else its
/// sign-in.
pub fn next_steps(
    known: &KnownCli,
    route: &zlo_client::orchestration::RouteProvider,
) -> Vec<(&'static str, &'static str)> {
    let sign_in = known.login.map(|c| ("Sign in", c));
    let install = known.install.map(|c| ("Install the CLI", c));
    if route.authenticated {
        Vec::new()
    } else if route.needs_native_cli() {
        known.install.map(|c| ("Install", c)).into_iter().collect()
    } else if route.launch_unknown() && known.legacy_runtime {
        sign_in.into_iter().chain(install).collect()
    } else {
        sign_in.into_iter().collect()
    }
}

/// The known CLI whose runtime id `runtime` starts with.
pub fn known_cli(runtime: &str) -> Option<&'static KnownCli> {
    KNOWN_CLIS.iter().find(|k| runtime.starts_with(k.runtime))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_legacy_runtime_offers_the_cli_install_not_its_sign_in() {
        use zlo_client::orchestration::{CatalogLaunch, RouteProvider};
        let agy = known_cli("antigravity").unwrap();
        let route = |authenticated, launch| RouteProvider {
            provider: "antigravity".into(),
            runtime: "antigravity".into(),
            authenticated,
            launch,
            ..RouteProvider::default()
        };
        let install = agy.install.unwrap();
        assert_eq!(
            next_steps(agy, &route(false, Some(CatalogLaunch::Legacy))),
            vec![("Install", install)]
        );
        assert_eq!(
            next_steps(agy, &route(false, Some(CatalogLaunch::Native))),
            vec![("Sign in", "agy acp --login")]
        );
        // Review P2: an older gateway sends no launch kind, so a signed-out
        // Antigravity may be the legacy runtime with no sign-in: both ways
        // are offered.
        for unknown in [None, Some(CatalogLaunch::Other)] {
            assert_eq!(
                next_steps(agy, &route(false, unknown)),
                vec![("Sign in", "agy acp --login"), ("Install the CLI", install)]
            );
        }
        assert!(next_steps(agy, &route(true, Some(CatalogLaunch::Legacy))).is_empty());
        assert!(next_steps(agy, &route(true, None)).is_empty());
        // A CLI with only one program keeps its one sign-in.
        let claude = known_cli("claude-cli").unwrap();
        let claude_route = RouteProvider {
            runtime: "claude-cli".into(),
            ..route(false, None)
        };
        assert_eq!(
            next_steps(claude, &claude_route),
            vec![("Sign in", "claude auth login")]
        );
    }

    #[test]
    fn names_and_kinds() {
        assert_eq!(display_name("openai_codex"), "ChatGPT plan");
        assert_eq!(display_name("some_new-host"), "Some New Host");
        assert!(kind_of("openai_codex") == Kind::Account);
        assert!(kind_of("lmstudio") == Kind::Local);
        assert!(kind_of("groq") == Kind::ApiKey);
    }
}
