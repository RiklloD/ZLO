//! The `/` and `@` composer menu, shared by a thread's composer and the
//! new-thread screen: its inventory, highlight, keys and popover.
//!
//! `@` lists mentions first (the Changes selection and git diff, files,
//! folders, recent threads; `ironclaw_gpui_client::mentions`), then tools
//! and skills. A picked mention becomes an atomic chip in the composer.

use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;

use ironclaw_gpui_client::commands::{
    MenuItem, MenuSources, complete_mention, menu_matches_with, tab_complete, trigger_token,
};
use ironclaw_gpui_client::mentions::{
    self, FileIndex, IndexCache, LineRange, MentionContext, MentionKind, RankedPaths, Ranking,
    ThreadEntry,
};
use ironclaw_gpui_client::product_wire::RebornProductCommandInfo;
use ironclaw_gpui_client::{Client, ClientError};

use crate::assets::ExtraIcon;
use crate::design::kit;
use crate::ui::*;
use gpui_kit::ScrollHandle;

/// File listings per project folder, shared by every composer.
#[derive(Default)]
struct MentionIndexes(IndexCache);

impl Global for MentionIndexes {}

#[derive(Default)]
pub(crate) struct ComposerMenu {
    pub(crate) sources: Rc<MenuSources>,
    index: usize,
    /// The draft the menu was closed for with Esc; typing reopens it.
    dismissed: Option<SharedString>,
    /// Keeps the highlighted row in view as ↑ ↓ move it.
    scroll: ScrollHandle,
    /// The project folder's listing, for `@` files and folders.
    files: Option<Arc<FileIndex>>,
    /// The folder the listing was last asked for: a listing of another
    /// folder that lands later is not applied.
    files_root: Option<PathBuf>,
    /// The latest ranking of `files`, made off the UI thread.
    ranked: Option<Arc<RankedPaths>>,
    /// The `(listing, query)` being ranked now, so it is not started twice.
    ranking: Option<(u64, String)>,
    /// The `@` query typed last, ranked once its listing lands.
    want_query: Option<String>,
    /// Lines picked in the Changes tab, offered as an `@` selection.
    pub(crate) selection: Option<(PathBuf, LineRange)>,
    /// The thread this composer belongs to, left out of `@` threads.
    pub(crate) current_thread: Option<String>,
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
        let ctx = MentionContext {
            index: self.files.as_deref(),
            threads: &[],
            selection: self
                .selection
                .as_ref()
                .map(|(path, range)| (path.as_path(), *range)),
            current_thread: self.current_thread.as_deref(),
            // Ranking a big listing never runs here, on the UI thread.
            ranking: match &self.ranked {
                Some(ranked) => Ranking::Ready(ranked),
                None => Ranking::Pending,
            },
        };
        menu_matches_with(&draft, &self.sources, &ctx)
    }

    /// The `@` query being typed, lowercase as the menu matches it.
    pub(crate) fn at_query(&self, composer: &Entity<TextareaState>, cx: &App) -> Option<String> {
        let draft = composer.read(cx).value();
        let (_, token) = trigger_token(&draft)?;
        token.strip_prefix('@').map(str::to_lowercase)
    }

    /// Point `@` files at `root`'s listing (or none).
    fn want_root(&mut self, root: Option<PathBuf>) {
        if self.files_root != root {
            self.files = None;
            self.ranked = None;
            self.ranking = None;
        }
        self.files_root = root;
    }

    /// Take a listing that landed: only the one for the folder last asked
    /// for. Returns whether it was taken.
    fn accept_index(&mut self, index: Arc<FileIndex>) -> bool {
        if self.files_root.as_deref() != Some(index.root.as_path()) {
            return false;
        }
        if self.files.as_ref().map(|f| f.generation) != Some(index.generation) {
            self.ranked = None;
            self.ranking = None;
        }
        self.files = Some(index);
        true
    }

    /// Take a ranking that landed: only one of the listing shown now.
    fn accept_ranking(&mut self, ranked: RankedPaths) -> bool {
        let current = self.files.as_ref().map(|f| f.generation);
        if self.ranking.as_ref().map(|(g, q)| (*g, q.as_str()))
            == Some((ranked.generation, ranked.query.as_str()))
        {
            self.ranking = None;
        }
        if current != Some(ranked.generation) {
            return false;
        }
        self.ranked = Some(Arc::new(ranked));
        true
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
                let item = &items[self.selected(len)];
                if let MenuItem::Mention(_) = item {
                    pick(item, composer, window, cx);
                } else {
                    let draft = composer.read(cx).value();
                    if let Some(text) = tab_complete(&draft, &items, self.selected(len)) {
                        set_text_at_end(composer, text, window, cx);
                    }
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
    if let MenuItem::Mention(entry) = item {
        insert_chip(composer, &entry.uri, &entry.label, window, cx);
        return MenuKey::Handled;
    }
    let text = complete_mention(&composer.read(cx).value(), item);
    set_text_at_end(composer, text, window, cx);
    MenuKey::Handled
}

/// Replace the `@word` being typed with a mention chip and a space.
fn insert_chip(
    composer: &Entity<TextareaState>,
    uri: &mentions::MentionUri,
    label: &str,
    window: &mut Window,
    cx: &mut App,
) {
    let draft = composer.read(cx).value();
    let start = trigger_token(&draft).map_or(draft.len(), |(start, _)| start);
    let chip = mentions::chip_text(label);
    let token = InlineToken::new(mentions::token_id(uri), chip.clone()).with_label(chip);
    composer.update(cx, |state, cx| {
        let placed = if start < draft.len() {
            state.replace_range_with_token(start..draft.len(), token, window, cx)
        } else {
            state.replace_with_token(token, window, cx)
        };
        match placed {
            Ok(()) => state.insert(" ", window, cx),
            Err(error) => tracing::debug!(%error, "composer: mention chip not placed"),
        }
        state.focus(window, cx);
    });
}

/// Point `@` files at `root`'s listing: the cached one at once, rebuilt in
/// the background when missing or stale. `menu_of` finds the menu again
/// when the listing lands; a listing of a folder no longer asked for is
/// dropped.
pub(crate) fn sync_index<V: 'static>(
    root: Option<&Path>,
    menu_of: fn(&mut V) -> Option<&mut ComposerMenu>,
    view: &mut V,
    cx: &mut Context<V>,
) {
    let root = root.map(Path::to_path_buf);
    if let Some(menu) = menu_of(view) {
        menu.want_root(root.clone());
    }
    let Some(root) = root else {
        return;
    };
    let (cached, rebuild) = cx.default_global::<MentionIndexes>().0.get(&root);
    if let (Some(index), Some(menu)) = (cached, menu_of(view)) {
        menu.accept_index(index);
    }
    if !rebuild {
        return;
    }
    cx.spawn(async move |this, cx| {
        let dir = root.clone();
        let index = cx
            .background_spawn(async move { FileIndex::build(&dir) })
            .await;
        let index_root = index.root.clone();
        let kept = this.update(cx, |view, cx| {
            cx.default_global::<MentionIndexes>().0.put(index);
            let (fresh, _) = cx.default_global::<MentionIndexes>().0.get(&index_root);
            let query = menu_of(view).and_then(|menu| {
                let taken = fresh.is_some_and(|index| menu.accept_index(index));
                taken.then(|| menu.want_query.clone()).flatten()
            });
            if let Some(query) = query {
                rank_in_background(query, menu_of, view, cx);
            }
            cx.notify();
        });
        if kept.is_err() {
            cx.update(|cx| cx.default_global::<MentionIndexes>().0.abandon(&root));
        }
    })
    .detach();
}

/// Rank the menu's listing for `query` off the UI thread; the menu shows
/// its previous ranking until this one lands.
pub(crate) fn rank_in_background<V: 'static>(
    query: String,
    menu_of: fn(&mut V) -> Option<&mut ComposerMenu>,
    view: &mut V,
    cx: &mut Context<V>,
) {
    let Some(menu) = menu_of(view) else {
        return;
    };
    let query = query.trim().to_string();
    menu.want_query = Some(query.clone());
    let Some(index) = menu.files.clone() else {
        return;
    };
    let key = (index.generation, query.clone());
    if menu
        .ranked
        .as_ref()
        .is_some_and(|r| r.is_for(&index, &query))
        || menu.ranking.as_ref() == Some(&key)
    {
        return;
    }
    menu.ranking = Some(key);
    cx.spawn(async move |this, cx| {
        let ranked = cx
            .background_spawn(async move { mentions::rank_paths(&index, &query) })
            .await;
        // silent-ok: the view closed while ranking
        let _ = this.update(cx, |view, cx| {
            if let Some(menu) = menu_of(view)
                && menu.accept_ranking(ranked)
            {
                cx.notify();
            }
        });
    })
    .detach();
}

/// Recent threads from the inbox list, newest first.
fn thread_entries(rows: Vec<ironclaw_gpui_client::inbox::InboxRow>) -> Vec<ThreadEntry> {
    let mut threads: Vec<ThreadEntry> = rows
        .into_iter()
        .filter_map(|row| {
            let title = row.title.filter(|t| !t.trim().is_empty())?;
            Some(ThreadEntry {
                thread_id: row.thread_id,
                title,
                last_activity_at: row.last_activity_at,
            })
        })
        .collect();
    threads.sort_by_key(|t| std::cmp::Reverse(t.last_activity_at));
    threads
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

/// Fetch the menu's inventory. Skills, extensions and threads only enrich
/// it, so a failed fetch leaves those sections empty.
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
    let threads = c.inbox().await.map(|l| l.rows).unwrap_or_default();
    Ok(MenuSources {
        threads: thread_entries(threads),
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
            .when_some(mention_icon(item), |row, icon| row.child(icon))
            .child(
                div()
                    .font_medium()
                    .flex_none()
                    .max_w(px(320.))
                    .truncate()
                    .child(item.label()),
            )
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

/// The leading icon of an `@` mention row.
fn mention_icon(item: &MenuItem) -> Option<Icon> {
    let MenuItem::Mention(entry) = item else {
        return None;
    };
    Some(kind_icon(entry.kind).xsmall())
}

/// The icon for a mention of `kind`, in the menu and on its chip.
pub(crate) fn kind_icon(kind: MentionKind) -> Icon {
    match kind {
        MentionKind::File => Icon::new(IconName::File),
        MentionKind::Folder => Icon::new(IconName::Folder),
        MentionKind::Thread => Icon::new(ExtraIcon::MessagesSquare),
        MentionKind::Changes => Icon::new(ExtraIcon::FileDiff),
    }
}

/// The section a mention belongs to, for its chip's icon.
pub(crate) fn uri_kind(uri: &mentions::MentionUri) -> MentionKind {
    use mentions::MentionUri as U;
    match uri {
        U::File { .. } => MentionKind::File,
        U::Directory { .. } => MentionKind::Folder,
        U::Thread { .. } => MentionKind::Thread,
        U::GitDiff { .. } | U::Selection { .. } | U::Skill { .. } | U::Extension { .. } => {
            MentionKind::Changes
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn listing(root: &str) -> Arc<FileIndex> {
        Arc::new(FileIndex {
            root: PathBuf::from(root),
            files: vec!["menu.rs".into()],
            generation: root.bytes().map(u64::from).sum(),
            ..FileIndex::default()
        })
    }

    /// A listing of folder A that lands after the menu moved to folder B is
    /// dropped, and so is a ranking of a listing no longer shown.
    #[test]
    fn late_results_for_another_folder_are_dropped() {
        let mut menu = ComposerMenu::default();
        menu.want_root(Some(PathBuf::from("/b")));
        assert!(!menu.accept_index(listing("/a")));
        assert!(menu.files.is_none());
        assert!(menu.accept_index(listing("/b")));
        let stale = mentions::rank_paths(&listing("/a"), "menu");
        assert!(!menu.accept_ranking(stale));
        assert!(menu.ranked.is_none());
        let shown = menu.files.clone().unwrap();
        assert!(menu.accept_ranking(mentions::rank_paths(&shown, "menu")));
        // Moving to another folder clears what was shown for the last one.
        menu.want_root(Some(PathBuf::from("/c")));
        assert!(menu.files.is_none() && menu.ranked.is_none());
    }
}
