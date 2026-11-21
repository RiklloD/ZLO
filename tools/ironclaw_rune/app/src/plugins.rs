//! Plugin UI slots: where plugins put their views, and the trees they sent.
//!
//! A plugin declares slots (`GET /plugin-ui/slots`); the app mounts each at
//! its place and draws the node tree the backend renders for it
//! (`plugin_ui` in `zlo_client`, drawn by [`render`]):
//!
//! | Slot | Mounted by | Context |
//! |---|---|---|
//! | `panel.tab` | a tab in the chat's right pane (`chat::side`, `Tab::Plugin`) | the open thread |
//! | `sidebar.session_row.badge` | a chip on each session row ([`badges`]) | that row's thread |
//! | `settings.page` | the plugin's entry in Settings → Plugins (`pages::extensions`) | none |
//!
//! Energy: nothing polls. Slots are read on connect and after a plugin's
//! lifecycle changes; a tree is rendered when its mount comes into view
//! (a tab picked, a plugin opened, a row scrolled in) and replaced by the
//! reply to a button or list press. A backend without plugins answers 404:
//! the app then has no slots and shows nothing.

pub mod badges;
pub mod fixtures;
pub mod render;

use std::collections::HashMap;

use rune::El;
use rune_kit::Kit;
use serde_json::Value;
use zlo_client::plugins::{Slot, SlotContext, SlotKind, UiReply, plain_reason};

use crate::jobs::{self, Generation};
use crate::state::State;

pub use render::Density;

/// One mounted slot instance: which slot, and for which thread.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct MountKey {
    pub extension_id: String,
    pub slot_id: String,
    pub thread_id: Option<String>,
}

impl MountKey {
    pub fn new(slot: &Slot, thread_id: Option<String>) -> Self {
        Self {
            extension_id: slot.extension_id.clone(),
            slot_id: slot.slot_id.clone(),
            thread_id,
        }
    }

    fn context(&self) -> SlotContext {
        SlotContext {
            thread_id: self.thread_id.clone(),
        }
    }

    /// The element-key prefix for this mount's tree.
    pub fn el_key(&self) -> String {
        format!(
            "plug-{}-{}-{}",
            self.extension_id,
            self.slot_id,
            self.thread_id.as_deref().unwrap_or("-")
        )
    }
}

/// A mount's last tree (or the plugin's reason it has none) and whether a
/// render or event is on its way.
#[derive(Default)]
pub struct Mount {
    pub view: Option<UiReply>,
    pub busy: bool,
    asked: Generation,
}

#[derive(Default)]
pub struct PluginsState {
    /// Slots at the mount points this app knows, in the backend's order.
    pub slots: Vec<Slot>,
    pub mounts: HashMap<MountKey, Mount>,
    pub badges: badges::Badges,
    /// Bumped each time the slot list is replaced: replies asked before
    /// are dropped.
    slots_gen: Generation,
    /// The latest slot-list request: only its reply is taken.
    slots_asked: Generation,
}

impl PluginsState {
    pub fn of(&self, kind: SlotKind) -> impl Iterator<Item = &Slot> {
        self.slots.iter().filter(move |s| s.slot == kind)
    }

    pub fn find(&self, extension_id: &str, slot_id: &str) -> Option<&Slot> {
        self.slots
            .iter()
            .find(|s| s.extension_id == extension_id && s.slot_id == slot_id)
    }

    /// Replace the slot list; trees and badges of the old one go.
    pub fn set_slots(&mut self, slots: Vec<Slot>) {
        self.slots = slots
            .into_iter()
            .filter(|s| s.slot != SlotKind::Other)
            .collect();
        self.mounts.clear();
        self.badges.clear();
        self.slots_gen.bump();
    }
}

/// Read the slot list: on connect and after a plugin's lifecycle changed.
pub fn refresh_slots(s: &mut State) {
    let asked = s.plugins.slots_asked.bump();
    jobs::run(
        s,
        "plugin slots",
        |c| async move { c.plugin_ui_slots().await },
        move |s, result, _| {
            // An earlier request answering late must not undo a later one.
            if s.plugins.slots_asked != asked {
                return;
            }
            match result {
                Ok(list) => s.plugins.set_slots(list.slots),
                Err(e) => {
                    // An older backend has no plugin UI: no slots, no noise.
                    if !e.is_not_found() {
                        tracing::debug!(error = %e, "plugin slots unavailable");
                    }
                    s.plugins.set_slots(Vec::new());
                }
            }
            crate::chat::side::plugin_slots_changed(s);
        },
    );
}

/// Render `key`'s tree now (its mount came into view). The last tree stays
/// up until the new one lands.
pub fn render(s: &mut State, key: MountKey) {
    send(s, key, None);
}

/// A button or list item was pressed: send its action, show the reply.
pub fn event(s: &mut State, key: MountKey, action: String, payload: Value) {
    send(s, key, Some((action, payload)));
}

fn send(s: &mut State, key: MountKey, event: Option<(String, Value)>) {
    let slots_gen = s.plugins.slots_gen;
    let mount = s.plugins.mounts.entry(key.clone()).or_default();
    let asked = mount.asked.bump();
    mount.busy = true;
    let job_key = key.clone();
    // A press's answer is the person's own: the tree's busy dim lifts and
    // what changed moves. A first render or a re-render lands at once.
    let pressed = event.is_some();
    jobs::run(
        s,
        "plugin ui",
        move |c| async move {
            let ctx = job_key.context();
            match event {
                Some((action, payload)) => {
                    c.plugin_ui_event(
                        &job_key.extension_id,
                        &job_key.slot_id,
                        &ctx,
                        &action,
                        &payload,
                    )
                    .await
                }
                None => {
                    c.plugin_ui_render(&job_key.extension_id, &job_key.slot_id, &ctx)
                        .await
                }
            }
        },
        move |s, result, tx| {
            if s.plugins.slots_gen != slots_gen {
                return;
            }
            let Some(mount) = s.plugins.mounts.get_mut(&key).filter(|m| m.asked == asked) else {
                return;
            };
            if pressed {
                tx.animated();
            }
            mount.busy = false;
            mount.view = Some(match result {
                Ok(reply) => reply,
                Err(e) => UiReply::Error(plain_reason(&e)),
            });
        },
    );
}

/// A mount's body: its tree, a quiet error, or a spinner while the first
/// render is on its way.
pub fn mount_view(s: &State, k: &Kit, key: &MountKey, density: Density) -> El {
    let p = &s.palette;
    let mount = s.plugins.mounts.get(key);
    let base = key.el_key();
    match mount.and_then(|m| m.view.as_ref()) {
        Some(UiReply::Tree(node)) => render::tree(s, k, key, node, density),
        Some(UiReply::Error(reason)) => render::error(p, &base, reason, density),
        None => render::loading(k, &base),
    }
}

#[cfg(test)]
mod ui_tests;

#[cfg(test)]
mod tests {
    use serde_json::json;
    use zlo_client::plugin_ui::Node;

    use super::*;

    fn slot(kind: &str) -> Slot {
        serde_json::from_value(json!({
            "extension_id": "linear", "slot_id": kind, "slot": kind, "title": "T", "icon": null
        }))
        .unwrap()
    }

    #[test]
    fn unknown_mount_points_are_dropped_and_a_new_list_clears_the_trees() {
        let mut p = PluginsState::default();
        p.set_slots(vec![slot("panel.tab"), slot("statusbar.item")]);
        assert_eq!(p.slots.len(), 1);
        let key = MountKey::new(&p.slots[0], Some("t1".into()));
        p.mounts.insert(
            key.clone(),
            Mount {
                view: Some(UiReply::Tree(Node::Divider)),
                ..Default::default()
            },
        );
        let before = p.slots_gen;
        p.set_slots(vec![slot("panel.tab")]);
        assert!(p.mounts.is_empty());
        assert_ne!(p.slots_gen, before, "replies asked before are dropped");
    }
}
