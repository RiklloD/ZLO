//! The `/` and `@` composer menu, shared by a thread's composer and the
//! new-thread screen: its inventory, highlight, keys and popover.

use std::rc::Rc;

use ironclaw_gpui_client::commands::{
    MenuItem, MenuSources, complete_mention, menu_matches, tab_complete,
};
use ironclaw_gpui_client::product_wire::RebornProductCommandInfo;
use ironclaw_gpui_client::{Client, ClientError};

use crate::design::kit;
use crate::ui::*;
use gpui_kit::ScrollHandle;

#[derive(Default)]
pub(crate) struct ComposerMenu {
    pub(crate) sources: Rc<MenuSources>,
    index: usize,
    /// The draft the menu was closed for with Esc; typing reopens it.
    dismissed: Option<SharedString>,
    /// Keeps the highlighted row in view as ↑ ↓ move it.
    scroll: ScrollHandle,
}

/// What a menu key did.
pub(crate) enum MenuKey {
    /// The menu is closed or the key isn't one of its keys.
    Unused,
    Handled,
    /// Enter on a command: the host runs it or fills in its arguments.
    Command(RebornProductCommandInfo),
}

impl ComposerMenu {
    /// Entries for the current draft, empty when the menu is closed.
    pub(crate) fn items(&self, composer: &Entity<TextareaState>, cx: &App) -> Vec<MenuItem> {
        let draft = composer.read(cx).value();
        if self.dismissed.as_ref() == Some(&draft) {
            return Vec::new();
        }
        menu_matches(&draft, &self.sources)
    }

    /// The highlighted row, clamped to `len`.
    pub(crate) fn selected(&self, len: usize) -> usize {
        self.index.min(len.saturating_sub(1))
    }

    /// The draft changed: the highlight goes back to the top.
    pub(crate) fn reset(&mut self) {
        self.index = 0;
        self.scroll.scroll_to_item(0);
    }

    pub(crate) fn scroll(&self) -> &ScrollHandle {
        &self.scroll
    }

    /// ↑ ↓ move the highlight, Tab fills in the highlighted row, Enter picks
    /// it, Esc closes the menu.
    pub(crate) fn key(
        &mut self,
        key: &str,
        composer: &Entity<TextareaState>,
        window: &mut Window,
        cx: &mut App,
    ) -> MenuKey {
        let items = self.items(composer, cx);
        if items.is_empty() {
            return MenuKey::Unused;
        }
        let len = items.len();
        match key {
            "up" => self.index = (self.selected(len) + len - 1) % len,
            "down" => self.index = (self.selected(len) + 1) % len,
            "tab" => {
                let draft = composer.read(cx).value();
                if let Some(text) = tab_complete(&draft, &items, self.selected(len)) {
                    set_text_at_end(composer, text, window, cx);
                }
            }
            "enter" => return pick(&items[self.selected(len)], composer, window, cx),
            "escape" => self.dismissed = Some(composer.read(cx).value()),
            _ => return MenuKey::Unused,
        }
        if matches!(key, "up" | "down") {
            self.scroll
                .scroll_to_item(child_index(&items, self.selected(len)));
        }
        MenuKey::Handled
    }
}

/// A skill or extension replaces the word being typed with its mention; a
/// command goes back to the host.
pub(crate) fn pick(
    item: &MenuItem,
    composer: &Entity<TextareaState>,
    window: &mut Window,
    cx: &mut App,
) -> MenuKey {
    if let MenuItem::Command(command) = item {
        return MenuKey::Command(command.clone());
    }
    let text = complete_mention(&composer.read(cx).value(), item);
    set_text_at_end(composer, text, window, cx);
    MenuKey::Handled
}

fn set_text_at_end(
    composer: &Entity<TextareaState>,
    text: String,
    window: &mut Window,
    cx: &mut App,
) {
    // Columns clamp to the line end, so this lands after the last word.
    let last_line = text.matches('\n').count() as u32;
    composer.update(cx, |state, cx| {
        state.set_value(text, window, cx);
        state.set_cursor_position(Position::new(last_line, u32::MAX), window, cx);
    });
}

/// Fetch the menu's inventory. Skills and extensions only enrich it, so a
/// failed fetch leaves those sections empty.
pub(crate) async fn load(c: Client, commands: bool) -> Result<MenuSources, ClientError> {
    let commands = if commands {
        c.list_commands().await?.commands
    } else {
        Vec::new()
    };
    let skills = c.skills().await.map(|l| l.skills).unwrap_or_default();
    let extensions = c
        .extensions()
        .await
        .map(|l| l.extensions)
        .unwrap_or_default();
    Ok(MenuSources {
        commands,
        skills: skills
            .into_iter()
            .map(|s| MenuItem::Skill {
                name: s.name,
                description: s.description,
            })
            .collect(),
        extensions: extensions
            .into_iter()
            .filter(|e| !e.tools.is_empty() && e.activation_error.is_none())
            .map(|e| MenuItem::Extension {
                name: e.package_ref.id.to_string(),
                description: e.display_name,
            })
            .collect(),
    })
}

/// Whether the list mixes sections, so each gets a heading.
fn mixed(items: &[MenuItem]) -> bool {
    items.windows(2).any(|w| w[0].section() != w[1].section())
}

/// Position of row `ix` among the popover's children, headings included.
fn child_index(items: &[MenuItem], ix: usize) -> usize {
    if !mixed(items) {
        return ix;
    }
    let headings = 1 + items[..=ix]
        .windows(2)
        .filter(|w| w[0].section() != w[1].section())
        .count();
    ix + headings
}

/// The menu popover, one heading per section. `on_pick` runs on a click.
pub(crate) fn render<V: 'static>(
    items: &[MenuItem],
    selected: usize,
    scroll: &ScrollHandle,
    on_pick: impl Fn(&mut V, &MenuItem, &mut Window, &mut Context<V>) + Clone + 'static,
    cx: &mut Context<V>,
) -> AnyElement {
    let pop = kit::popover("command-menu-pop", cx);
    let sections = items.iter().map(MenuItem::section).collect::<Vec<_>>();
    // A lone section needs no heading.
    let mixed = mixed(items);
    let mut rows: Vec<AnyElement> = Vec::new();
    for (ix, item) in items.iter().enumerate() {
        if mixed && (ix == 0 || sections[ix - 1] != sections[ix]) {
            rows.push(
                div()
                    .px_2()
                    .pt_1p5()
                    .pb_0p5()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child(item.section())
                    .into_any_element(),
            );
        }
        let pick = item.clone();
        let on_pick = on_pick.clone();
        rows.push(
            kit::row(
                SharedString::from(format!("menu-{}-{}", item.section(), item.name())),
                ix == selected,
                cx,
            )
            .role(Role::Button)
            .aria_label(item.label())
            .gap_3()
            .child(div().font_medium().flex_none().child(item.label()))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child(item.description().to_string()),
            )
            .on_click(cx.listener(move |this, _, window, cx| on_pick(this, &pick, window, cx)))
            .into_any_element(),
        );
    }
    pop(v_flex().w_full().child(
        div()
            .w_full()
            .max_h(px(280.))
            .id("command-menu")
            .track_scroll(scroll)
            .overflow_y_scroll()
            .p_1()
            .children(rows),
    ))
}
