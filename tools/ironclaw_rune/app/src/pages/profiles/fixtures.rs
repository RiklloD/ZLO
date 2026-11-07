//! Scene fixtures for the Profiles page (`scenes.rs`).

use super::*;

fn fixture_profile(id: &str, name: &str, used_hours_ago: Option<i64>, threads: u64) -> ProfileInfo {
    let now = chrono::Utc::now().timestamp_millis();
    ProfileInfo {
        profile_id: id.into(),
        settings: ProfileSettings {
            display_name: name.into(),
            soul: format!("You are {name}. Work carefully, explain briefly."),
            ..ProfileSettings::default()
        },
        is_default: id == "default",
        effective_folder: format!("~/.ironclaw/profiles/{id}"),
        git_branch: None,
        thread_count: threads,
        last_used_at: used_hours_ago.map(|h| now - h * 3_600_000),
        created_at: now - 30 * 86_400_000,
        updated_at: now - 86_400_000,
    }
}

fn fixture_catalog(ps: &mut ProfilesState) {
    let provider = |id: &str, name: &str, models: &[&str]| {
        ironclaw_gpui_client::orchestration::RouteProvider {
            provider: id.into(),
            name: name.into(),
            runtime: id.into(),
            authenticated: true,
            models: models.iter().map(|m| m.to_string()).collect(),
            ..Default::default()
        }
    };
    ps.catalog.providers = vec![
        provider(
            "claude_cli",
            "Claude",
            &["claude-opus-4-5", "claude-sonnet-4-5", "claude-haiku-4-5"],
        ),
        provider("codex", "Codex", &["gpt-5.1-codex", "gpt-5.1"]),
    ];
    ps.catalog.tools = vec![
        ("shell.exec".into(), "Run a shell command".into()),
        ("files.read".into(), "Read a file".into()),
        ("files.write".into(), "Write a file".into()),
        ("web.fetch".into(), "Fetch a URL".into()),
    ];
    ps.catalog.skills = vec![("deploy".into(), "Ship to staging".into())];
    ps.catalog.mcp = vec![
        ("github".into(), "GitHub".into()),
        ("linear".into(), "Linear".into()),
    ];
}

/// The list: recently used and never used profiles, an import notice.
pub fn fixture_list(s: &mut State) {
    let ps = &mut s.pages.profiles;
    fixture_catalog(ps);
    let mut work = fixture_profile("work", "Work", Some(2), 14);
    work.settings.default_provider = Some("claude_cli".into());
    work.settings.default_model = Some("claude-opus-4-5".into());
    work.git_branch = Some("rune-port".into());
    let mut research = fixture_profile("research", "Deep Research", Some(30), 3);
    research.settings.default_provider = Some("gemini".into());
    ps.items = vec![
        work,
        research,
        fixture_profile("default", "Default", Some(80), 52),
        fixture_profile("home", "Home Lab", None, 0),
    ];
    ps.last_used = Some("work".into());
    ps.loaded = true;
    ps.notice = Some("Imported 1: home. Skipped: coder (already exists).".into());
}

/// The editor on "Work" with the tools checklist open.
pub fn fixture_pickers(s: &mut State) {
    fixture_list(s);
    select(s, Some("work".into()));
    let ps = &mut s.pages.profiles;
    ps.picks.tools.insert("files.read".into());
    ps.picks.tools.insert("old.tool".into());
    ps.menu = Some(Menu::Tools);
}

/// The editor on "Work", no menu open.
pub fn fixture_editor(s: &mut State) {
    fixture_list(s);
    select(s, Some("work".into()));
}
