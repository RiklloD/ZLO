//! Fixture slots and trees for the plugin scenes, decoded from wire-shaped
//! JSON so they follow the contract (`zlo_client::plugins`).

use serde_json::{Value, json};
use zlo_client::plugins::{SlotList, UiReply};

use super::badges::{BadgeKey, Entry};
use super::{Mount, MountKey};
use crate::state::State;

/// A light or dark scheme for a scene, from default preferences.
pub fn scheme(s: &mut State, light: bool) {
    s.prefs.scheme = if light { "light" } else { "dark" }.into();
    s.palette = crate::design::resolve(&s.prefs, !light);
}

/// The Linear plugin's three slots.
pub fn slots(s: &mut State) {
    let list = json!({ "slots": [
        { "extension_id": "linear", "slot_id": "issues", "slot": "panel.tab", "title": "Linear", "icon": "target" },
        { "extension_id": "linear", "slot_id": "state", "slot": "sidebar.session_row.badge", "title": "Linear issue", "icon": null },
        { "extension_id": "linear", "slot_id": "about", "slot": "settings.page", "title": "Workspace", "icon": null }
    ]});
    let list: SlotList =
        crate::pages::extensions::fixtures::decode("plugin slots", list).unwrap_or_default();
    s.plugins.set_slots(list.slots);
}

/// Put `tree` (wire JSON, `{ "tree": .. }` or `{ "error": .. }`) on a mount.
pub fn put(s: &mut State, slot_id: &str, thread_id: Option<&str>, reply: Value) {
    let Some(slot) = s.plugins.find("linear", slot_id).cloned() else {
        return;
    };
    let view = match UiReply::from_json(&reply.to_string()) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(error = %e, "plugin fixture does not decode");
            return;
        }
    };
    s.plugins.mounts.insert(
        MountKey::new(&slot, thread_id.map(str::to_string)),
        Mount {
            view: Some(view),
            ..Default::default()
        },
    );
}

/// `panel.tab`: the issue this chat works on, its checks and the queue.
pub fn panel_tree() -> Value {
    json!({ "tree": { "type": "col", "gap": 10, "children": [
        { "type": "row", "gap": 8, "children": [
            { "type": "text", "text": "ENG-412", "style": "mono", "tone": "neutral" },
            { "type": "badge", "text": "In review", "tone": "accent" }
        ]},
        { "type": "text", "text": "Port the frontend to Rune", "style": "title" },
        { "type": "text", "text": "Every page keeps its scenes and the energy budget holds.", "style": "body" },
        { "type": "row", "gap": 6, "children": [
            { "type": "button", "label": "Mark done", "action": "close", "payload": { "id": "ENG-412" }, "style": "primary" },
            { "type": "button", "label": "Open in Linear", "action": "open", "payload": { "id": "ENG-412" }, "style": "secondary" },
            { "type": "button", "label": "Unlink", "action": "unlink", "style": "ghost" }
        ]},
        { "type": "divider" },
        { "type": "text", "text": "Checks", "style": "caption" },
        { "type": "list", "items": [
            { "title": "Scenes render", "subtitle": "62 of 62", "trailing": { "type": "badge", "text": "Pass", "tone": "success" } },
            { "title": "Energy budget", "subtitle": "0.4 wakes/s idle", "trailing": { "type": "badge", "text": "Watch", "tone": "warning" } },
            { "title": "Clippy", "trailing": { "type": "badge", "text": "2 warnings", "tone": "danger" } }
        ]},
        { "type": "text", "text": "Up next", "style": "caption" },
        { "type": "list", "items": [
            { "title": "ENG-415 Dock overflow at narrow widths", "subtitle": "Todo · Lorik", "action": "open", "payload": { "id": "ENG-415" } },
            { "title": "ENG-420 Plugin settings page", "subtitle": "In progress", "action": "open", "payload": { "id": "ENG-420" } }
        ]},
        { "type": "markdown", "source": "**Note.** Plugin views use the app's own components; `style` and `tone` are all a plugin picks." },
        { "type": "sparkles", "intensity": 11 }
    ]}})
}

/// `settings.page`: the plugin's workspace page.
pub fn page_tree() -> Value {
    json!({ "tree": { "type": "col", "gap": 12, "children": [
        { "type": "row", "gap": 8, "children": [
            { "type": "text", "text": "Connected to Acme", "style": "title" },
            { "type": "badge", "text": "Healthy", "tone": "success" }
        ]},
        { "type": "text", "text": "Signed in as lorik@acme.dev. Issues sync every time a chat links one.", "style": "caption" },
        { "type": "list", "items": [
            { "title": "Engineering", "subtitle": "ENG · 214 open", "trailing": { "type": "text", "text": "Default", "style": "caption" } },
            { "title": "Design", "subtitle": "DES · 31 open" }
        ]},
        { "type": "row", "gap": 8, "children": [
            { "type": "button", "label": "Sync now", "action": "sync", "style": "secondary" },
            { "type": "button", "label": "Sign out", "action": "sign_out", "style": "ghost" }
        ]}
    ]}})
}

/// Badges on the fixture shell's rows; one row's render failed (no chip).
pub fn badges(s: &mut State) {
    let picks = [
        (
            "t-pin",
            Some(("ENG-412", zlo_client::plugin_ui::Tone::Accent)),
        ),
        (
            "t-needs",
            Some(("Blocked", zlo_client::plugin_ui::Tone::Danger)),
        ),
        (
            "t-done",
            Some(("Done", zlo_client::plugin_ui::Tone::Success)),
        ),
        (
            "t-work",
            Some(("ENG-415", zlo_client::plugin_ui::Tone::Neutral)),
        ),
        ("t-plain", None),
    ];
    for (thread, badge) in picks {
        let stamp = s
            .workspace
            .inbox
            .row(thread)
            .map_or(0, |r| r.last_activity_at);
        s.plugins.badges.cache.insert(
            BadgeKey {
                extension_id: "linear".into(),
                slot_id: "state".into(),
                thread_id: thread.into(),
            },
            Entry {
                stamp,
                badge: badge.map(|(t, tone)| (t.to_string(), tone)),
                pending: false,
            },
        );
    }
}

/// The chat with its right pane on the Linear tab, and badges in the
/// sidebar.
pub fn panel(s: &mut State, light: bool) {
    crate::workspace::fixtures::fixture_shell(s);
    s.prefs = Default::default();
    scheme(s, light);
    slots(s);
    put(s, "issues", Some("t-pin"), panel_tree());
    badges(s);
    s.prefs.side_open = true;
    s.prefs.side_tab = "plugin:linear/issues".into();
    s.prefs.side_width = 380.;
}

/// The Linear tab when the plugin answered with a reason instead.
pub fn panel_error(s: &mut State) {
    panel(s, false);
    put(
        s,
        "issues",
        Some("t-pin"),
        json!({ "error": "Linear did not answer. Check the connection and refresh." }),
    );
}
