//! The `/` and `@` composer menu, shared by a thread's composer and the
//! new-thread screen: its inventory, highlight, keys and popover (GPUI
//! `chat/menu.rs`).
//!
//! `@` lists mentions first (the Changes selection, files, folders, recent
//! threads; `zlo_client::mentions`), then tools and skills. A
//! picked mention becomes an atomic pill in the composer.
//!
//! A composer owns a [`ComposerMenu`] and finds it again through a
//! [`MenuOf`] (its slot in `State`), so file listings and rankings that land
//! later reach the right menu, or nothing if it is gone.

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;

use rune::{El, Icon, TextEdit, Tx, icon};
use rune_kit::{Kit, PopupItem, PopupList};
use zlo_client::commands::{
    MenuItem, MenuSources, complete_mention, menu_matches_with, tab_complete, trigger_token,
};
use zlo_client::mentions::{
    self, FileIndex, IndexCache, LineRange, MentionContext, MentionKind, RankedPaths, Ranking,
    ThreadEntry,
};
use zlo_client::product_wire::RebornProductCommandInfo;
use zlo_client::{Client, ClientError};

use crate::icons;
use crate::jobs;
use crate::state::State;

thread_local! {
    /// File listings per project folder (or remote thread workspace),
    /// shared by every composer.
    static INDEXES: RefCell<IndexCache<IndexKey>> = RefCell::default();
}

/// Where a composer's `@` files come from.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum IndexKey {
    /// A folder on this Mac, walked here.
    Local(PathBuf),
    /// A thread's workspace on a remote backend, listed by that backend
    /// (`/files/index`); its mentions name `/workspace/...`, which the
    /// backend resolves under the folder it owns. Keyed by computer and
    /// thread, since every thread's workspace is `/workspace`.
    Remote { environment: String, thread: String },
}

/// What a click on a menu row does.
pub type OnPick = Rc<dyn Fn(&mut State, &MenuItem, &mut Tx)>;

/// Finds a composer's menu again from the root state.
pub type MenuOf = Arc<dyn for<'a> Fn(&'a mut State) -> Option<&'a mut ComposerMenu> + Send + Sync>;

#[derive(Default)]
pub struct ComposerMenu {
    pub sources: Rc<MenuSources>,
    index: usize,
    /// The draft the menu was closed for with Esc; typing reopens it.
    dismissed: Option<String>,
    /// The project folder's listing, for `@` files and folders.
    files: Option<Arc<FileIndex>>,
    /// The listing last asked for: a listing of another folder (or thread)
    /// that lands later is not applied.
    files_key: Option<IndexKey>,
    /// The latest ranking of `files`, made off the UI thread.
    ranked: Option<Arc<RankedPaths>>,
    /// The `(listing, query)` being ranked now, so it is not started twice.
    ranking: Option<(u64, String)>,
    /// The `@` query typed last, ranked once its listing lands.
    want_query: Option<String>,
    /// Lines picked in the Changes tab, offered as an `@` selection.
    pub selection: Option<(PathBuf, LineRange)>,
    /// The thread this composer belongs to, left out of `@` threads.
    pub current_thread: Option<String>,
}

/// What a menu key did.
pub enum MenuKey {
    /// The menu is closed or the key isn't one of its keys.
    Unused,
    Handled,
    /// Enter on a command: the host runs it or fills in its arguments.
    Command(RebornProductCommandInfo),
}

impl ComposerMenu {
    /// Entries for the current draft, empty when the menu is closed.
    pub fn items(&self, draft: &str) -> Vec<MenuItem> {
        if self.dismissed.as_deref() == Some(draft) {
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
        menu_matches_with(draft, &self.sources, &ctx)
    }

    /// The `@` query being typed, lowercase as the menu matches it.
    pub fn at_query(draft: &str) -> Option<String> {
        let (_, token) = trigger_token(draft)?;
        token.strip_prefix('@').map(str::to_lowercase)
    }

    /// Point `@` files at `key`'s listing (or none).
    fn want_key(&mut self, key: Option<IndexKey>) {
        if self.files_key != key {
            self.files = None;
            self.ranked = None;
            self.ranking = None;
        }
        self.files_key = key;
    }

    /// Take a listing that landed: only the one last asked for. Returns
    /// whether it was taken.
    fn accept_index(&mut self, key: &IndexKey, index: Arc<FileIndex>) -> bool {
        if self.files_key.as_ref() != Some(key) {
            return false;
        }
        if self.files.as_ref().map(|f| f.generation) != Some(index.generation) {
            self.ranked = None;
            self.ranking = None;
        }
        self.files = Some(index);
        true
    }

    /// Take a ranking that landed: only one of the listing shown now, for
    /// the query typed now (an older query's ranking may land later).
    fn accept_ranking(&mut self, ranked: RankedPaths) -> bool {
        let current = self.files.as_ref().map(|f| f.generation);
        if self.ranking.as_ref().map(|(g, q)| (*g, q.as_str()))
            == Some((ranked.generation, ranked.query.as_str()))
        {
            self.ranking = None;
        }
        if current != Some(ranked.generation)
            || self.want_query.as_deref() != Some(ranked.query.as_str())
        {
            return false;
        }
        self.ranked = Some(Arc::new(ranked));
        true
    }

    /// A scene's `@` files: `files` under `root`, ranked for `query` now
    /// (a real menu lists and ranks off the UI thread).
    pub fn fixture_files(&mut self, root: &Path, files: Vec<String>, query: &str) {
        let index = Arc::new(FileIndex {
            root: root.to_path_buf(),
            files,
            generation: 1,
            ..FileIndex::default()
        });
        let key = IndexKey::Local(root.to_path_buf());
        self.want_key(Some(key.clone()));
        self.accept_index(&key, index.clone());
        self.want_query = Some(query.to_string());
        self.accept_ranking(mentions::rank_paths(&index, query));
    }

    /// The highlighted row, clamped to `len`.
    pub fn selected(&self, len: usize) -> usize {
        self.index.min(len.saturating_sub(1))
    }

    /// The draft changed: the highlight goes back to the top.
    pub fn reset(&mut self) {
        self.index = 0;
    }

    /// ↑ ↓ move the highlight (scrolled into view), Tab fills in the
    /// highlighted row, Enter picks it, Esc closes the menu.
    pub fn key(&mut self, key: MenuKeyName, composer: &mut TextEdit, tx: &mut Tx) -> MenuKey {
        let items = self.items(&composer.text);
        if items.is_empty() {
            return MenuKey::Unused;
        }
        let len = items.len();
        match key {
            MenuKeyName::Up => self.index = (self.selected(len) + len - 1) % len,
            MenuKeyName::Down => self.index = (self.selected(len) + 1) % len,
            MenuKeyName::Tab => {
                let item = &items[self.selected(len)];
                if let MenuItem::Mention(_) = item {
                    pick(item, composer);
                } else if let Some(text) = tab_complete(&composer.text, &items, self.selected(len))
                {
                    set_text_at_end(composer, text);
                }
            }
            MenuKeyName::Enter => return pick(&items[self.selected(len)], composer),
            MenuKeyName::Escape => self.dismissed = Some(composer.text.clone()),
        }
        // `PopupList` scrolls the highlighted row into view.
        let _ = tx;
        MenuKey::Handled
    }
}

/// The keys the menu takes.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum MenuKeyName {
    Up,
    Down,
    Tab,
    Enter,
    Escape,
}

impl MenuKeyName {
    /// The menu key a composer key is, if any (no modifiers but Shift-less).
    pub fn of(ev: &rune::KeyEvent) -> Option<Self> {
        let m = ev.mods;
        if m.cmd || m.ctrl || m.alt || m.shift {
            return None;
        }
        Some(match ev.key {
            rune::Key::Up => Self::Up,
            rune::Key::Down => Self::Down,
            rune::Key::Tab => Self::Tab,
            rune::Key::Enter => Self::Enter,
            rune::Key::Escape => Self::Escape,
            _ => return None,
        })
    }
}

/// A skill or extension replaces the word being typed with its mention; a
/// command goes back to the host.
pub fn pick(item: &MenuItem, composer: &mut TextEdit) -> MenuKey {
    if let MenuItem::Command(command) = item {
        return MenuKey::Command(command.clone());
    }
    if let MenuItem::Mention(entry) = item {
        insert_chip(composer, &entry.uri, &entry.label);
        return MenuKey::Handled;
    }
    let text = complete_mention(&composer.text, item);
    set_text_at_end(composer, text);
    MenuKey::Handled
}

/// Replace the `@word` being typed with a mention pill and a space.
fn insert_chip(composer: &mut TextEdit, uri: &mentions::MentionUri, label: &str) {
    let draft = composer.text.clone();
    let start = trigger_token(&draft).map_or(draft.len(), |(start, _)| start);
    let chip = mentions::chip_text(label);
    let id = mentions::token_id(uri);
    composer.undo_group(|e| {
        e.replace_with_pill(start..draft.len(), id, &chip);
        e.insert(" ");
    });
    composer.focused = true;
}

fn set_text_at_end(composer: &mut TextEdit, text: String) {
    // `replace_text` leaves the caret after the last word.
    composer.replace_text(text);
    composer.mark_edited();
}

/// Point `@` files at `key`'s listing: the cached one at once, rebuilt in
/// the background when missing or stale (walked here for a local folder,
/// asked of the backend for a remote thread). A listing no longer asked
/// for is dropped.
pub fn sync_index(s: &mut State, key: Option<IndexKey>, menu_of: MenuOf) {
    if let Some(menu) = menu_of(s) {
        menu.want_key(key.clone());
    }
    let Some(key) = key else {
        return;
    };
    let (cached, rebuild) = INDEXES.with(|c| c.borrow_mut().get(&key));
    if let (Some(index), Some(menu)) = (cached, menu_of(s)) {
        menu.accept_index(&key, index);
    }
    if !rebuild {
        return;
    }
    match key.clone() {
        IndexKey::Local(dir) => jobs::blocking(
            s,
            move || FileIndex::build(&dir),
            move |s, index, _| landed(s, key, Some(index), menu_of),
        ),
        IndexKey::Remote { thread, .. } => jobs::run(
            s,
            "mention index",
            move |c| async move { c.thread_file_index(&thread).await },
            move |s, result, _| {
                // silent-ok: an older backend has no index route; `@` then
                // offers threads, skills and commands, as before.
                let index = result.map(remote_listing).ok();
                landed(s, key, index, menu_of);
            },
        ),
    }
}

/// A remote backend's index as a listing rooted at its workspace path, so
/// a picked file mentions `/workspace/<path>`.
pub fn remote_listing(index: zlo_client::workspace_views::RebornProjectFsIndexResponse) -> FileIndex {
    use zlo_client::workspace_views::ProjectFsEntryKind;
    let files = index
        .entries
        .into_iter()
        .filter(|e| e.kind == ProjectFsEntryKind::File)
        .map(|e| e.path)
        .collect();
    FileIndex::from_listing(PathBuf::from(index.root), files, !index.complete)
}

/// A listing landed (or failed): cache it, show it if still wanted, rank it.
fn landed(s: &mut State, key: IndexKey, index: Option<FileIndex>, menu_of: MenuOf) {
    let Some(index) = index else {
        INDEXES.with(|c| c.borrow_mut().abandon(&key));
        return;
    };
    INDEXES.with(|c| c.borrow_mut().put_as(key.clone(), index));
    let (fresh, _) = INDEXES.with(|c| c.borrow_mut().get(&key));
    let query = menu_of(s).and_then(|menu| {
        let taken = fresh.is_some_and(|index| menu.accept_index(&key, index));
        taken.then(|| menu.want_query.clone()).flatten()
    });
    match query {
        Some(query) => rank_in_background(s, query, menu_of),
        None if menu_of(s).is_none() => {
            INDEXES.with(|c| c.borrow_mut().abandon(&key));
        }
        None => {}
    }
}

/// Rank the menu's listing for `query` off the UI thread; the menu shows
/// its previous ranking until this one lands.
pub fn rank_in_background(s: &mut State, query: String, menu_of: MenuOf) {
    let Some(menu) = menu_of(s) else {
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
    jobs::blocking(
        s,
        move || mentions::rank_paths(&index, &query),
        move |s, ranked, _| {
            // silent-ok: the composer closed while ranking
            if let Some(menu) = menu_of(s) {
                menu.accept_ranking(ranked);
            }
        },
    );
}

/// Recent threads from the inbox list, newest first.
fn thread_entries(rows: Vec<zlo_client::inbox::InboxRow>) -> Vec<ThreadEntry> {
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

/// Fetch the menu's inventory. Skills, extensions and threads only enrich
/// it, so a failed fetch leaves those sections empty.
pub async fn load(c: Client, commands: bool) -> Result<MenuSources, ClientError> {
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

fn row_key(item: &MenuItem) -> String {
    format!("menu-{}-{}", item.section(), item.name())
}

/// Where the menu floats: rune-kit's `PopupList` key, the element it
/// hangs off (the composer), on which side, and how wide.
pub struct Placing {
    pub key: &'static str,
    pub anchor: String,
    pub side: rune::Side,
    pub width: f32,
    /// The menu, found again from the root (hover moves its highlight).
    pub menu_of: MenuOf,
}

/// The `@` `/` menu as rune-kit's `PopupList` over the composer: one row
/// per item (a mention's kind glyph before it, its description beside it)
/// and, when sections mix, a dimmed heading row before each. Focus stays in
/// the composer, whose keys move the highlight (`ComposerMenu::key`); the
/// pointer moves it too, and a click runs `on_pick`. No scrim: a click
/// elsewhere (into the composer) leaves it open, as before; Esc closes it.
pub fn popup(
    s: &State,
    k: &Kit,
    layer: &mut Vec<El>,
    at: Placing,
    items: &[MenuItem],
    selected: usize,
    on_pick: OnPick,
) {
    let menu_of = at.menu_of.clone();
    let p = &s.palette;
    let mixed = mixed(items);
    // Popup row → item index (`None` for a heading).
    let mut rows: Vec<Option<usize>> = Vec::new();
    let mut entries = Vec::new();
    for (ix, item) in items.iter().enumerate() {
        if mixed && (ix == 0 || items[ix - 1].section() != item.section()) {
            rows.push(None);
            entries.push(
                PopupItem::new(format!("head-{}", item.section()), item.section()).disabled(true),
            );
        }
        rows.push(Some(ix));
        let mut entry = PopupItem::new(row_key(item), item.label()).description(item.description());
        if let Some(glyph) = mention_icon(item) {
            entry = entry.leading(icon(glyph, 12.).color(p.text_muted));
        }
        entries.push(entry);
    }
    let active = rows.iter().position(|r| *r == Some(selected));
    let rows = Rc::new(rows);
    let picks: Rc<Vec<MenuItem>> = Rc::new(items.to_vec());
    PopupList::new(at.key, at.anchor)
        .side(at.side)
        .offset(6.)
        .width(at.width)
        .max_height(280.)
        .padding(4.)
        .row_radius(crate::design::CONTROL_RADIUS)
        .items(entries)
        .active(active)
        .label("Suggestions")
        .on_active(move |s: &mut State, row| {
            if let (Some(Some(ix)), Some(m)) = (rows.get(row), menu_of(s)) {
                m.index = *ix;
            }
        })
        .on_choose(move |s: &mut State, id: &str, tx| {
            if let Some(item) = picks.iter().find(|i| row_key(i) == id) {
                on_pick(s, item, tx);
            }
        })
        .build(k, layer);
}

/// The leading icon of an `@` mention row.
fn mention_icon(item: &MenuItem) -> Option<&'static Icon> {
    let MenuItem::Mention(entry) = item else {
        return None;
    };
    Some(kind_icon(entry.kind))
}

/// The icon for a mention of `kind`, in the menu and on its pill.
pub fn kind_icon(kind: MentionKind) -> &'static Icon {
    match kind {
        MentionKind::File => &icons::FILE,
        MentionKind::Folder => &icons::FOLDER,
        MentionKind::Thread => &icons::MESSAGES_SQUARE,
        MentionKind::Changes => &icons::FILE_DIFF,
    }
}

/// The section a mention belongs to, for its pill's icon.
pub fn uri_kind(uri: &mentions::MentionUri) -> MentionKind {
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

    fn local(root: &str) -> IndexKey {
        IndexKey::Local(PathBuf::from(root))
    }

    /// Every remote thread's listing is rooted at `/workspace`: the key, not
    /// the root, tells them apart, so another thread's (or computer's)
    /// listing landing late is dropped, and a remote listing's files
    /// mention workspace paths.
    #[test]
    fn remote_listings_are_kept_apart_by_computer_and_thread() {
        use zlo_client::workspace_views::{
            ProjectFsEntryKind, ProjectFsIndexEntry, RebornProjectFsIndexResponse,
        };
        let remote = |env: &str, thread: &str| IndexKey::Remote {
            environment: env.into(),
            thread: thread.into(),
        };
        let index = Arc::new(remote_listing(RebornProjectFsIndexResponse {
            root: "/workspace".into(),
            entries: vec![
                ProjectFsIndexEntry {
                    path: "src".into(),
                    kind: ProjectFsEntryKind::Directory,
                },
                ProjectFsIndexEntry {
                    path: "src/menu.rs".into(),
                    kind: ProjectFsEntryKind::File,
                },
            ],
            complete: false,
        }));
        assert_eq!(index.root, PathBuf::from("/workspace"));
        assert_eq!(index.files, ["src/menu.rs"]);
        assert_eq!(index.dirs, ["src"]);
        assert!(index.truncated);
        let mut menu = ComposerMenu::default();
        menu.want_key(Some(remote("studio", "t2")));
        assert!(!menu.accept_index(&remote("studio", "t1"), index.clone()));
        assert!(!menu.accept_index(&remote("laptop", "t2"), index.clone()));
        assert!(!menu.accept_index(&local("/workspace"), index.clone()));
        assert!(menu.accept_index(&remote("studio", "t2"), index.clone()));
        let rows = mentions::mention_matches(
            "menu",
            &MentionContext {
                index: menu.files.as_deref(),
                ranking: Ranking::Inline,
                ..MentionContext::default()
            },
        );
        assert!(
            rows.iter().any(|r| r.uri
                == mentions::MentionUri::File {
                    path: "/workspace/src/menu.rs".into(),
                    line_range: None,
                }),
            "{rows:?}"
        );
    }

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
        menu.want_key(Some(local("/b")));
        assert!(!menu.accept_index(&local("/a"), listing("/a")));
        assert!(menu.files.is_none());
        assert!(menu.accept_index(&local("/b"), listing("/b")));
        let stale = mentions::rank_paths(&listing("/a"), "menu");
        assert!(!menu.accept_ranking(stale));
        assert!(menu.ranked.is_none());
        let shown = menu.files.clone().unwrap();
        menu.want_query = Some("menu".into());
        assert!(menu.accept_ranking(mentions::rank_paths(&shown, "menu")));
        // Moving to another folder clears what was shown for the last one.
        menu.want_key(Some(local("/c")));
        assert!(menu.files.is_none() && menu.ranked.is_none());
    }

    /// Typing `@me` then `@men` ranks both; the `@me` ranking landing last
    /// must not replace the `@men` one.
    #[test]
    fn an_older_querys_ranking_never_overwrites_a_newer_one() {
        let mut menu = ComposerMenu::default();
        menu.want_key(Some(local("/b")));
        assert!(menu.accept_index(&local("/b"), listing("/b")));
        let shown = menu.files.clone().unwrap();
        menu.want_query = Some("me".into());
        menu.want_query = Some("men".into());
        assert!(menu.accept_ranking(mentions::rank_paths(&shown, "men")));
        assert!(!menu.accept_ranking(mentions::rank_paths(&shown, "me")));
        assert_eq!(menu.ranked.as_ref().unwrap().query, "men");
    }

    /// Picking an `@` file swaps the typed query for one pill and a space.
    #[test]
    fn a_picked_mention_becomes_a_pill() {
        let mut edit = TextEdit::new("look at @men");
        let uri = mentions::MentionUri::File {
            path: "/p/menu.rs".into(),
            line_range: None,
        };
        insert_chip(&mut edit, &uri, "menu.rs");
        let pills: Vec<_> = edit.pills().map(|(r, id)| (r, id.to_string())).collect();
        assert_eq!(pills.len(), 1);
        assert_eq!(pills[0].1, mentions::token_id(&uri));
        assert!(edit.text.starts_with("look at "));
        assert!(edit.text.ends_with(' '));
    }
}
