//! What the thread details card can show, and which of it the user picked
//! (t3code's `threadDetailsItems.ts` and `threadDetailsCardLayout.ts`).
//!
//! t3code's "Project scripts" item is left out: ZLO keeps no per-project
//! script store, so there is nothing for it to list.

use std::collections::BTreeMap;

/// One item of the card, in t3code's order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Item {
    Environment,
    OpenIn,
    Branch,
    GitActions,
    Changes,
    Servers,
    Automations,
    Lineage,
    SideChats,
    Usage,
    Context,
}

pub struct ItemDef {
    pub item: Item,
    /// Stored in `Prefs::thread_details_items`: never rename one.
    pub id: &'static str,
    pub label: &'static str,
    pub default_visible: bool,
}

const fn def(item: Item, id: &'static str, label: &'static str, on: bool) -> ItemDef {
    ItemDef {
        item,
        id,
        label,
        default_visible: on,
    }
}

/// Every item, in the order the card and its menu list them.
pub const ITEMS: [ItemDef; 11] = [
    def(
        Item::Environment,
        "environment",
        "Environment and worktree",
        true,
    ),
    def(Item::OpenIn, "openIn", "Open in editor", true),
    def(Item::Branch, "branch", "Branch and pull request", true),
    def(Item::GitActions, "gitActions", "Git actions", true),
    def(Item::Changes, "changes", "Changes", true),
    def(Item::Servers, "servers", "Servers", true),
    def(Item::Automations, "automations", "Automations", true),
    def(Item::Lineage, "lineage", "Lineage", true),
    def(Item::SideChats, "sideChats", "Side chats", true),
    def(Item::Usage, "usage", "Usage limits", false),
    def(Item::Context, "context", "Context window", false),
];

fn def_of(item: Item) -> &'static ItemDef {
    ITEMS.iter().find(|d| d.item == item).unwrap_or(&ITEMS[0])
}

/// Which items show, from the stored choices.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Visibility([bool; ITEMS.len()]);

impl Visibility {
    pub fn get(&self, item: Item) -> bool {
        ITEMS
            .iter()
            .position(|d| d.item == item)
            .is_some_and(|ix| self.0[ix])
    }

    /// Every item at its default.
    pub fn is_default(&self) -> bool {
        ITEMS
            .iter()
            .zip(self.0)
            .all(|(d, on)| on == d.default_visible)
    }
}

pub fn resolve(stored: &BTreeMap<String, bool>) -> Visibility {
    let mut out = [false; ITEMS.len()];
    for (slot, d) in out.iter_mut().zip(&ITEMS) {
        *slot = stored.get(d.id).copied().unwrap_or(d.default_visible);
    }
    Visibility(out)
}

/// Show or hide `item`, keeping only choices that differ from its default.
pub fn set(stored: &mut BTreeMap<String, bool>, item: Item, visible: bool) {
    let d = def_of(item);
    if visible == d.default_visible {
        stored.remove(d.id);
    } else {
        stored.insert(d.id.to_string(), visible);
    }
}

/// How much the card shows for the height it has (t3code's densities).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Density {
    /// Everything picked.
    Full,
    /// No environment row, servers, automations or relationships; the git
    /// quick action only while it works.
    Compact,
    /// Compact without "Open in" either.
    Essential,
}

/// The fullest density whose content fits in `height`.
pub fn density(height: f32, full: f32, compact: f32) -> Density {
    if full <= height {
        Density::Full
    } else if compact <= height {
        Density::Compact
    } else {
        Density::Essential
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_show_all_but_usage_and_context() {
        let v = resolve(&BTreeMap::new());
        assert!(v.is_default());
        assert!(v.get(Item::Environment) && v.get(Item::SideChats));
        assert!(!v.get(Item::Usage) && !v.get(Item::Context));
    }

    /// Only choices that differ from the default are stored, so turning an
    /// item back to its default leaves nothing behind.
    #[test]
    fn only_non_defaults_are_stored() {
        let mut stored = BTreeMap::new();
        set(&mut stored, Item::Usage, true);
        set(&mut stored, Item::Servers, false);
        assert_eq!(
            stored,
            BTreeMap::from([("servers".to_string(), false), ("usage".to_string(), true)])
        );
        let v = resolve(&stored);
        assert!(v.get(Item::Usage) && !v.get(Item::Servers) && !v.is_default());
        set(&mut stored, Item::Usage, false);
        set(&mut stored, Item::Servers, true);
        assert!(stored.is_empty());
    }

    #[test]
    fn ids_are_unique_and_match_t3code() {
        let ids: Vec<_> = ITEMS.iter().map(|d| d.id).collect();
        assert_eq!(
            ids,
            [
                "environment",
                "openIn",
                "branch",
                "gitActions",
                "changes",
                "servers",
                "automations",
                "lineage",
                "sideChats",
                "usage",
                "context"
            ]
        );
    }

    #[test]
    fn density_steps_down_as_height_shrinks() {
        assert_eq!(density(600., 500., 300.), Density::Full);
        assert_eq!(density(400., 500., 300.), Density::Compact);
        assert_eq!(density(200., 500., 300.), Density::Essential);
    }
}
