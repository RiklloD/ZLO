//! Fixture data for the Skills scene (`--scene skills`).

use serde_json::json;

use super::CatalogSkill;
use crate::pages::extensions::fixtures::decode;
use crate::state::{Conn, Page, State};

fn skill(name: &str, description: &str, source: &str, editable: bool) -> serde_json::Value {
    json!({
        "name": name,
        "description": description,
        "version": "1.0.0",
        "trust": if editable { "trusted" } else { "installed" },
        "source": source,
        "source_kind": source,
        "can_edit": editable,
        "can_delete": editable,
        "auto_activate": name != "release-notes"
    })
}

pub fn skills(s: &mut State) {
    s.fixture = true;
    s.conn = Conn::Ready;
    s.page = Page::Skills;
    let list = json!([
        skill(
            "commit",
            "Write a conventional commit message for the staged diff.",
            "system",
            false
        ),
        skill(
            "release-notes",
            "Draft release notes from merged pull requests.",
            "user",
            true
        ),
        skill(
            "rust-review",
            "Review Rust changes for panics, error handling and clippy.",
            "installed",
            true
        )
    ]);
    let sk = &mut s.pages.skills;
    sk.loaded = true;
    sk.skills = decode::<Vec<_>>("skills", list).unwrap_or_default();
    sk.auto_learned = Some(true);
    sk.notice = Some("Installed rust-review.".into());
    sk.query = rune::TextEdit::new("review");
    sk.results = Some(Ok(vec![
        CatalogSkill {
            name: "rust-review".into(),
            description: "Review Rust changes for panics, error handling and clippy.".into(),
        },
        CatalogSkill {
            name: "ts-review".into(),
            description: "Review TypeScript changes for type holes and async mistakes.".into(),
        },
    ]));
    sk.open.insert(
        "release-notes".into(),
        Some(
            "# Release notes\n\nCollect the **merged** pull requests since the last tag, then:\n\n\
             1. Group them by area.\n2. Write one line per change, user-facing first.\n\n\
             Run `git log --merges` for the list."
                .into(),
        ),
    );
    sk.confirm_remove = Some("rust-review".into());
}

/// The skills page with release-notes' SKILL.md open in the mono editor.
pub fn skills_editor(s: &mut State) {
    skills(s);
    let sk = &mut s.pages.skills;
    sk.notice = None;
    sk.confirm_remove = None;
    let text = sk.open.get("release-notes").cloned().flatten().unwrap_or_default();
    sk.editor_text = rune::TextEdit::new(text);
    sk.editor = Some("release-notes".into());
}
