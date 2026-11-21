//! Browser: one web page beside the chat, for `localhost` previews, docs and
//! anything the agent links to (GPUI `chat/side/browser.rs`). The page is a
//! `WKWebView` from `native_view`, created when this tab shows and gone
//! soon after it hides. There is one page for the whole app, so switching
//! threads keeps it.

use std::cell::RefCell;
use std::collections::HashSet;
use std::time::{Duration, Instant};

use rune::{Cursor, El, TextEdit, Tx, col, el, icon, row};
use rune_kit::{Kit, TextInput};
use zlo_client::transcript::Row;

use super::ui::{self, xs, xs_med};
use super::{Tab, bridge, side, side_mut, side_of, side_total, tab_shown, thread_id};
use crate::design::motion;
use crate::icons;
use crate::jobs;
use crate::native_view::{self, PageChange, Slot, Source, WebCommand, WebState};
use crate::state::State;

thread_local! {
    /// The address last asked for; the page may have moved on since.
    static PAGE: RefCell<Option<String>> = const { RefCell::new(None) };
}

pub(super) fn page() -> Option<String> {
    PAGE.with(|p| p.borrow().clone())
}

fn short_host(url: &str) -> Option<String> {
    let rest = url.split_once("://").map(|(_, r)| r).unwrap_or(url);
    let host = rest.split(['/', '?', '#']).next()?.trim();
    if host.is_empty() {
        None
    } else {
        Some(host.strip_prefix("www.").unwrap_or(host).to_string())
    }
}

fn short_url_parts(url: &str) -> (String, String) {
    let host = short_host(url).unwrap_or_else(|| url.to_string());
    let rest = url.split_once("://").map(|(_, r)| r).unwrap_or(url);
    let path = rest
        .split_once('/')
        .map(|(_, p)| format!("/{p}"))
        .filter(|p| p != "/")
        .unwrap_or_default();
    (host, path)
}

#[derive(Default)]
pub(super) struct BrowserState {
    input: TextEdit,
    /// The page state the toolbar last drew.
    shown: Option<WebState>,
    /// Recently visited URLs in this thread, newest first.
    recent: Vec<String>,
    /// A load just finished: the progress bar holds full width for
    /// [`PROGRESS_HOLD`] before it goes.
    finishing: bool,
}

/// How long a finished load's bar shows full before it goes.
const PROGRESS_HOLD: Duration = Duration::from_millis(200);

/// Show `url` in the Browser tab.
pub(super) fn go(s: &mut State, url: String) {
    let source = Source::Url(url.clone());
    PAGE.with(|p| *p.borrow_mut() = Some(url.clone()));
    if let Some(x) = side_mut(s) {
        x.browser.input.replace_text(url.clone());
        x.browser.recent.retain(|u| u != &url);
        x.browser.recent.insert(0, url);
        x.browser.recent.truncate(5);
    }
    native_view::load(Slot::Browser, &source);
    super::show(s, Tab::Browser);
    start(s);
}

/// Close the page: its view goes now, the tab shows the launchpad again.
fn close(s: &mut State) {
    PAGE.with(|p| p.borrow_mut().take());
    native_view::close(Slot::Browser);
    if let Some(x) = side_mut(s) {
        x.browser.shown = None;
        x.browser.input.replace_text("");
    }
}

/// Read the page into the bar now: its tab just showed (what changed
/// while it was hidden was not read), or a page was just asked for.
pub(super) fn start(s: &mut State) {
    if page().is_some() && tab_shown(s, Tab::Browser) {
        apply(s, native_view::web_state());
    }
}

/// The app's poll: the page's observer (`native_view`) noticed a change,
/// so read it, at most four times a second. An idle page notices nothing
/// and costs nothing; there is no timer. Returns whether the bar changed.
pub(super) fn poll(s: &mut State, tx: &mut Tx) -> bool {
    poll_with(s, tx, Instant::now(), native_view::web_state)
}

fn poll_with(
    s: &mut State,
    tx: &mut Tx,
    now: Instant,
    read: impl FnOnce() -> Option<WebState>,
) -> bool {
    match native_view::take_page_change(now) {
        PageChange::None => false,
        // Read too recently: one timed wake brings this read.
        PageChange::After(wait) => {
            tx.wake_after(wait);
            false
        }
        // Behind another tab the page is read when the tab shows.
        PageChange::Read if page().is_some() && tab_shown(s, Tab::Browser) => apply(s, read()),
        PageChange::Read => false,
    }
}

/// Put page state `state` in the bar: the address (unless the person is
/// typing in it), Back/Forward, and the load bar, which holds full for
/// [`PROGRESS_HOLD`] once a load ends. Its commits land at once: they
/// follow the page, not something the person did this frame, and
/// `tx.animated()` would animate whatever else lands in the same frame.
fn apply(s: &mut State, state: Option<WebState>) -> bool {
    let Some(thread) = thread_id(s) else {
        return false;
    };
    let Some(x) = side_mut(s) else { return false };
    let mut changed = false;
    if let Some(url) = state
        .as_ref()
        .map(|w| w.url.clone())
        .filter(|u| !u.is_empty())
        && !x.browser.input.focused
        && x.browser.input.text != url
    {
        x.browser.input.replace_text(url);
        changed = true;
    }
    if state == x.browser.shown {
        return changed;
    }
    let was_loading = x.browser.shown.as_ref().is_some_and(|w| w.loading);
    let loading_now = state.as_ref().is_some_and(|w| w.loading);
    x.browser.shown = state;
    if loading_now {
        x.browser.finishing = false;
    } else if was_loading {
        x.browser.finishing = true;
        jobs::after(s, PROGRESS_HOLD, move |s, _| {
            if let Some(x) = side_of(s, &thread) {
                x.browser.finishing = false;
            }
        });
    }
    true
}

/// The load bar's fill, 0 to 1, while it shows: the page's progress while
/// it loads, full for a moment once done.
pub(super) fn progress(s: &State) -> Option<f64> {
    let x = side(s)?;
    page()?;
    if x.browser.finishing {
        return Some(1.);
    }
    x.browser
        .shown
        .as_ref()
        .filter(|w| w.loading)
        .map(|w| w.progress.clamp(0.05, 1.))
}

/// The 2 pt load bar along the top of the Browser's page, under the tab
/// switcher: it follows the page's progress and goes a moment after the
/// load is done. Its steps come from the poll, so they land at once; the
/// motion plays only on a commit the person caused (a load they started
/// mounting it). `width` is the dock's.
pub(super) fn progress_bar(s: &State, width: f32) -> Option<El> {
    let fill = progress(s)? as f32;
    Some(
        el().key("web-progress")
            .absolute(0., -1.)
            .h(2.)
            .w(width * fill)
            .fill(s.palette.accent)
            .motion(crate::design::motion::PILL)
            .transition(rune::Transition::opacity()),
    )
}

/// Extract deduplicated `http://` / `https://` links mentioned in the
/// conversation, newest first (up to 5), so the user can open them in one click.
fn extract_urls(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < text.len() {
        let rest = &text[i..];
        let pos = rest
            .find("https://")
            .into_iter()
            .chain(rest.find("http://"))
            .min();
        let Some(rel) = pos else { break };
        let start = i + rel;
        let tail = &text[start..];
        let end = tail
            .find(|c: char| c.is_whitespace() || matches!(c, '"' | '\'' | '<' | '>' | '`'))
            .unwrap_or(tail.len());
        let raw = &tail[..end];
        let trimmed = raw.trim_end_matches(|c: char| {
            matches!(c, '.' | ',' | ';' | ':' | '!' | '?' | ')' | ']' | '}')
        });
        if trimmed.len() > "https://x".len() && short_host(trimmed).is_some() {
            out.push(trimmed.to_string());
        }
        i = start + end.max(1);
    }
    out
}

fn thread_links(s: &State) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for row in bridge::rows(s).iter().rev() {
        let content = match row {
            Row::Assistant { text, .. } | Row::User { text, .. } => text.as_str(),
            _ => continue,
        };
        for url in extract_urls(content).into_iter().rev() {
            if seen.insert(url.clone()) {
                out.push(url);
                if out.len() >= 5 {
                    return out;
                }
            }
        }
    }
    out
}

fn link_row(s: &State, key: String, url: String) -> El {
    let p = &s.palette;
    let (host, path) = short_url_parts(&url);
    let target = url.clone();
    row()
        .key(key)
        .role("button")
        .label(format!("Open {url}"))
        .help(url)
        .w_full()
        .min_h(40.)
        .px(10.)
        .py(6.)
        .gap(10.)
        .items_center()
        .radius(10.)
        .fill(p.surface_card.alpha(if p.light { 0.8 } else { 0.55 }))
        .border(1., p.border)
        .hover(p.hover)
        .hover_motion(motion::HOVER, motion::HOVER)
        .press_scale(motion::PRESS_CARD)
        .hover_group()
        .cursor(Cursor::Pointer)
        .child(
            el().size(26., 26.)
                .radius(7.)
                .center()
                .fill(p.accent_wash)
                .child(icon(&icons::GLOBE, 14.).color(p.accent)),
        )
        .child(
            col()
                .grow(1.)
                .min_w(0.)
                .gap(1.)
                .child(ui::line(host, xs_med(), p.text))
                .children((!path.is_empty()).then(|| ui::line(path, ui::tiny(), p.text_faint))),
        )
        .child(
            icon(&icons::ARROW_UP_RIGHT, 14.)
                .color(p.text_faint)
                .group_color(p.text),
        )
        .on_press(move |s: &mut State| go(s, target.clone()))
}

fn launchpad(s: &State, recent: &[String]) -> El {
    let p = &s.palette;
    let links = thread_links(s);
    let mut stack = col().w_full().px(12.).pt(12.).gap(12.);

    if !links.is_empty() {
        let mut group = col()
            .w_full()
            .gap(6.)
            .child(ui::section_label(s, "Links in this thread"));
        for (ix, url) in links.into_iter().enumerate() {
            group = group.child(link_row(s, format!("web-thread-link-{ix}"), url));
        }
        stack = stack.child(group);
    }

    // 2x2 quick-launch grid for common local dev servers.
    const SERVERS: [(&str, &str, &str); 4] = [
        ("localhost:3000", "Next · React", "http://localhost:3000"),
        ("localhost:5173", "Vite", "http://localhost:5173"),
        ("localhost:8080", "Local HTTP", "http://localhost:8080"),
        ("localhost:4000", "Dev Server", "http://localhost:4000"),
    ];
    let tile = |ix: usize, host: &'static str, hint: &'static str, url: &'static str| {
        row()
            .key(format!("web-quick-{ix}"))
            .role("button")
            .label(format!("Open {host}"))
            .grow(1.)
            .min_w(0.)
            .h(46.)
            .px(10.)
            .gap(8.)
            .items_center()
            .radius(10.)
            .fill(p.surface_card.alpha(if p.light { 0.8 } else { 0.55 }))
            .border(1., p.border)
            .hover(p.hover)
            .hover_motion(motion::HOVER, motion::HOVER)
            .press_scale(motion::PRESS_CARD)
            .hover_group()
            .cursor(Cursor::Pointer)
            .child(
                el().size(26., 26.).radius(7.).center().fill(p.hover).child(
                    icon(&icons::ZAP, 13.)
                        .color(p.text_muted)
                        .group_color(p.accent),
                ),
            )
            .child(
                col()
                    .grow(1.)
                    .min_w(0.)
                    .gap(1.)
                    .child(ui::line(host, xs_med(), p.text))
                    .child(ui::line(hint, ui::tiny(), p.text_faint)),
            )
            .on_press(move |s: &mut State| go(s, url.to_string()))
    };
    stack = stack.child(
        col()
            .w_full()
            .gap(6.)
            .child(ui::section_label(s, "Local dev servers"))
            .child(
                row()
                    .w_full()
                    .gap(6.)
                    .child(tile(0, SERVERS[0].0, SERVERS[0].1, SERVERS[0].2))
                    .child(tile(1, SERVERS[1].0, SERVERS[1].1, SERVERS[1].2)),
            )
            .child(
                row()
                    .w_full()
                    .gap(6.)
                    .child(tile(2, SERVERS[2].0, SERVERS[2].1, SERVERS[2].2))
                    .child(tile(3, SERVERS[3].0, SERVERS[3].1, SERVERS[3].2)),
            ),
    );

    if !recent.is_empty() {
        let mut group = col().w_full().gap(6.).child(ui::section_label(s, "Recent"));
        for (ix, url) in recent.iter().enumerate() {
            group = group.child(link_row(s, format!("web-recent-{ix}"), url.clone()));
        }
        stack = stack.child(group);
    }

    stack = stack.child(
        row()
            .w_full()
            .px(10.)
            .py(8.)
            .gap(8.)
            .items_center()
            .radius(9.)
            .fill(p.surface_card.alpha(if p.light { 0.55 } else { 0.35 }))
            .child(icon(&icons::INFO, 13.).color(p.text_faint))
            .child(
                ui::para(
                    "Type a URL or search in the bar above. Click links in chat to preview here, or ⌘-click to open externally.",
                    xs(),
                    p.text_faint,
                )
                .grow(1.)
                .min_w(0.),
            ),
    );

    ui::scroll("web-launchpad", stack)
}

/// The browser's bar (back, forward, reload, address, open outside, close),
/// drawn in the dock's header in place of a "Browser" title the tab pill
/// already shows, so the page gets the row back.
pub(super) fn bar(s: &State, k: &Kit) -> El {
    let Some(x) = side(s) else {
        return row().grow(1.);
    };
    let state = x.browser.shown.clone().unwrap_or_default();
    let url = page();
    let nav = |key: &str, i: &'static rune::Icon, tip: &str, on: bool, cmd: WebCommand| {
        ui::ibtn(i, tip)
            .disabled(!on)
            .on_press(move |_: &mut State| native_view::web(cmd))
            .build(k)
            .no_window_drag()
            .key(key.to_string())
    };
    let open_url = url.clone();
    let mut toolbar = row()
        .key("web-bar")
        .grow(1.)
        .min_w(0.)
        .gap(2.)
        .items_center()
        .no_window_drag();
    // The header is narrow: page controls show only while a page is open,
    // Forward only when there is somewhere to go, so the address keeps room.
    if url.is_some() {
        toolbar = toolbar.child(nav(
            "web-back",
            &icons::ARROW_LEFT,
            "Back",
            state.back,
            WebCommand::Back,
        ));
        if state.forward {
            toolbar = toolbar.child(nav(
                "web-forward",
                &icons::ARROW_RIGHT,
                "Forward",
                true,
                WebCommand::Forward,
            ));
        }
        toolbar = toolbar.child(if state.loading {
            nav("web-stop", &icons::CLOSE, "Stop", true, WebCommand::Stop)
        } else {
            nav(
                "web-reload",
                &icons::REFRESH_CW,
                "Reload",
                true,
                WebCommand::Reload,
            )
        });
    } else {
        toolbar = toolbar.child(icon(&icons::GLOBE, 13.5).color(s.palette.text_muted));
    }
    toolbar = toolbar.child(
        row().grow(1.).min_w(0.).px(2.).child(
            TextInput::new(&x.browser.input)
                .width(None)
                .placeholder("Search or enter URL")
                .label("Address")
                .on_edit(|s: &mut State| &mut side_total(s).browser.input)
                .on_submit(|s: &mut State, _| {
                    let text = side_total(s).browser.input.text.trim().to_string();
                    if let Some(url) = address(&text) {
                        go(s, url);
                    }
                })
                .build(k)
                .no_window_drag()
                .key("web-address"),
        ),
    );
    if url.is_some() {
        toolbar = toolbar
            .child(
                ui::ibtn(&icons::EXTERNAL_LINK, "Open in your browser")
                    .on_press_tx(move |_: &mut State, tx| {
                        let current = native_view::web_state()
                            .map(|s| s.url)
                            .filter(|u| !u.is_empty())
                            .or_else(|| open_url.clone());
                        if let Some(url) = current
                            && let Err(e) = tx.open_url(&url)
                        {
                            tracing::warn!(error = %e, "page not opened in the browser");
                        }
                    })
                    .build(k)
                    .no_window_drag()
                    .key("web-external"),
            )
            .child(
                ui::ibtn(&icons::CIRCLE_X, "Close page")
                    .on_press(close)
                    .build(k)
                    .no_window_drag()
                    .key("web-close"),
            );
    }
    toolbar
}

pub(super) fn view(s: &State, k: &Kit) -> El {
    let Some(x) = side(s) else {
        return ui::loading(k);
    };
    match page() {
        // Keyed apart, so opening or closing a page crossfades the
        // launchpad with the page (which fades in natively).
        Some(_) => ui::fill_col()
            .key("web-page")
            .child(native_view::surface(Slot::Browser)),
        None => ui::fill_col()
            .key("web-launchpad")
            .child(launchpad(s, &x.browser.recent)),
    }
}

/// What the address field means: a URL, a bare host, or a search.
fn address(text: &str) -> Option<String> {
    if text.is_empty() {
        return None;
    }
    if text.contains("://") {
        return Some(text.to_string());
    }
    let host = text.split(['/', ':']).next().unwrap_or_default();
    let local = host == "localhost"
        || host.ends_with(".local")
        || host.ends_with(".localhost")
        || host.split('.').count() == 4 && host.split('.').all(|p| p.parse::<u8>().is_ok());
    if local {
        return Some(format!("http://{text}"));
    }
    if !text.contains(' ') && host.contains('.') {
        return Some(format!("https://{text}"));
    }
    let query: String = text
        .bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (b as char).to_string()
            }
            b' ' => "+".into(),
            _ => format!("%{b:02X}"),
        })
        .collect();
    Some(format!("https://duckduckgo.com/?q={query}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The open chat `thread` with the Browser tab showing and a page.
    fn browsing(thread: &str) -> State {
        let mut s = crate::chat::fixtures::test_state();
        s.chat = Some(crate::chat::ChatState::new(thread.into()));
        s.prefs.side_open = true;
        s.prefs.side_tab = Tab::Browser.id().into();
        PAGE.with(|p| *p.borrow_mut() = Some("http://localhost:3000".into()));
        s
    }

    fn page_at(url: &str, loading: bool) -> Option<WebState> {
        Some(WebState {
            url: url.into(),
            loading,
            progress: 0.4,
            back: true,
            ..WebState::default()
        })
    }

    /// The observer's notification wakes the app once; the poll reads the
    /// page and the bar follows it: address, Back, load bar.
    #[test]
    fn a_page_change_is_read_once_into_the_bar() {
        let mut s = browsing("t1");
        let t0 = Instant::now();
        let mut tx = Tx::default();
        assert!(
            !poll_with(&mut s, &mut tx, t0, || panic!("idle: nothing to read")),
            "an idle page reads nothing"
        );
        assert!(native_view::note_page_change(), "the first change wakes");
        assert!(!native_view::note_page_change(), "the next ones do not");
        let read = poll_with(&mut s, &mut tx, t0, || {
            page_at("http://localhost:3000/next", true)
        });
        assert!(read);
        let x = side(&s).expect("side");
        assert_eq!(x.browser.input.text, "http://localhost:3000/next");
        assert!(x.browser.shown.as_ref().is_some_and(|w| w.back));
        assert_eq!(progress(&s), Some(0.4));
        assert!(
            !poll_with(&mut s, &mut tx, t0, || panic!("read already")),
            "one notification, one read"
        );
    }

    /// A page that changes itself (a redirect after a while, `pushState`
    /// every second) is followed without any input, and never read more
    /// than four times a second.
    #[test]
    fn a_page_changing_itself_is_followed_at_most_four_times_a_second() {
        let mut s = browsing("t1");
        let t0 = Instant::now();
        let mut tx = Tx::default();
        native_view::note_page_change();
        assert!(poll_with(&mut s, &mut tx, t0, || page_at(
            "http://a/1",
            false
        )));
        native_view::note_page_change();
        let soon = t0 + Duration::from_millis(50);
        assert!(
            !poll_with(&mut s, &mut tx, soon, || panic!("too soon")),
            "read 50 ms ago: wait"
        );
        assert!(
            format!("{tx:?}").contains("wake"),
            "one timed wake brings the read"
        );
        assert!(!native_view::note_page_change(), "no wake while one is set");
        let later = t0 + Duration::from_millis(260);
        assert!(poll_with(&mut s, &mut tx, later, || page_at(
            "http://a/2",
            false
        )));
        assert_eq!(
            side(&s).map(|x| x.browser.input.text.clone()).as_deref(),
            Some("http://a/2")
        );
    }

    /// Behind another tab the change is dropped; showing the tab reads it.
    #[test]
    fn a_hidden_browser_reads_nothing_until_it_shows() {
        let mut s = browsing("t1");
        s.prefs.side_tab = Tab::Activity.id().into();
        native_view::note_page_change();
        let mut tx = Tx::default();
        assert!(!poll_with(&mut s, &mut tx, Instant::now(), || panic!(
            "hidden"
        )));
    }

    /// The load bar follows the page's progress while it loads, shows full
    /// once the load ends, and goes when the hold clears.
    #[test]
    fn the_load_bar_follows_progress_then_shows_full_before_it_goes() {
        let mut s = browsing("t1");
        assert_eq!(progress(&s), None, "no load seen yet: no bar");
        assert!(apply(&mut s, page_at("http://a", true)));
        assert_eq!(progress(&s), Some(0.4));
        assert!(progress_bar(&s, 300.).is_some());
        assert!(apply(&mut s, page_at("http://a", false)));
        assert_eq!(progress(&s), Some(1.), "done: full for a moment");
        if let Some(x) = side_mut(&mut s) {
            x.browser.finishing = false;
        }
        assert_eq!(progress(&s), None, "then it goes");
        assert!(!apply(&mut s, page_at("http://a", false)), "unchanged");
    }

    #[test]
    fn addresses_resolve_like_a_browser_bar() {
        assert_eq!(
            address("localhost:3000").as_deref(),
            Some("http://localhost:3000")
        );
        assert_eq!(
            address("127.0.0.1:8080/x").as_deref(),
            Some("http://127.0.0.1:8080/x")
        );
        assert_eq!(
            address("docs.rs/gpui").as_deref(),
            Some("https://docs.rs/gpui")
        );
        assert_eq!(
            address("file:///tmp/a.html").as_deref(),
            Some("file:///tmp/a.html")
        );
        assert_eq!(
            address("rust lifetimes").as_deref(),
            Some("https://duckduckgo.com/?q=rust+lifetimes")
        );
        assert_eq!(address(""), None);
    }

    #[test]
    fn extract_urls_trims_punctuation_and_finds_multiple() {
        let urls = extract_urls(
            "Check https://app.notion.com/p/abc123, or preview (http://localhost:3000/home).",
        );
        assert_eq!(
            urls,
            vec![
                "https://app.notion.com/p/abc123",
                "http://localhost:3000/home",
            ]
        );
    }
}
