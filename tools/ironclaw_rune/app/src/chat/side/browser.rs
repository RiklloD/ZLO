//! Browser: one web page beside the chat, for `localhost` previews, docs and
//! anything the agent links to (GPUI `chat/side/browser.rs`). The page is a
//! `WKWebView` from `native_view`, created when this tab shows and gone
//! soon after it hides. There is one page for the whole app, so switching
//! threads keeps it.

use std::cell::RefCell;
use std::collections::HashSet;
use std::time::{Duration, Instant};

use rune::{Cursor, El, TextEdit, col, el, icon, row};
use rune_kit::{Kit, TextInput};
use zlo_client::transcript::Row;

use super::ui::{self, xs, xs_med};
use super::{Tab, bridge, side, side_mut, side_of, side_total, tab_shown, thread_id};
use crate::icons;
use crate::jobs;
use crate::native_view::{self, Slot, Source, WebCommand, WebState};
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
    polling: bool,
    /// The running poll loop; a tick of an older loop (or of another
    /// thread's side state) ends instead of doubling this one.
    poll: jobs::Generation,
    /// Recently visited URLs in this thread, newest first.
    recent: Vec<String>,
    /// A load just finished: the progress bar holds full width for
    /// [`PROGRESS_HOLD`] before it goes.
    finishing: bool,
    /// The person just acted on the page (a toolbar button, a click or a
    /// keystroke in it): the poll follows it until then even while nothing
    /// loads, to catch a navigation that is about to start or an in-page
    /// address change.
    watch_until: Option<Instant>,
}

/// How long the poll follows the page after the person acts on it.
const WATCH_FOR: Duration = Duration::from_secs(2);
/// The poll's pace while it runs: a load in progress, or [`WATCH_FOR`].
const POLL_EVERY: Duration = Duration::from_millis(250);

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

/// Follow the page while the tab shows and something can be changing it:
/// a load in progress, or for [`WATCH_FOR`] after the person acted on it
/// (this call). A loaded page at rest is not polled at all: what moves it
/// next is the person, and their input on the page restarts this
/// ([`page_input`]). The loop belongs to the thread that started it, under
/// a fresh generation; it ends once the tab hides.
pub(super) fn start(s: &mut State) {
    if page().is_none() {
        return;
    }
    let Some(thread) = thread_id(s) else { return };
    let Some(x) = side_mut(s) else { return };
    x.browser.watch_until = Some(Instant::now() + WATCH_FOR);
    if x.browser.polling {
        return;
    }
    x.browser.polling = true;
    let generation = x.browser.poll.bump();
    tick(s, thread, generation, POLL_EVERY);
}

/// The person clicked or typed in the page (`native_view`'s event monitor):
/// follow it for a moment, if its tab shows.
pub(super) fn page_input(s: &mut State) {
    if tab_shown(s, Tab::Browser) {
        start(s);
    }
}

/// A toolbar button: drive the page and follow what it does.
fn command(s: &mut State, cmd: WebCommand) {
    native_view::web(cmd);
    start(s);
}

/// One poll tick. Its commit lands instantly: it is a poll, not something
/// the person did this frame, and `tx.animated()` here would animate
/// whatever else lands in the same frame. The load bar steps with it.
fn tick(s: &State, thread: String, generation: jobs::Generation, wait: Duration) {
    jobs::after(s, wait, move |s, _| {
        if let Some(wait) = step(s, &thread, generation, Instant::now()) {
            tick(s, thread.clone(), generation, wait);
        }
        let done = side_of(s, &thread).is_some_and(|x| x.browser.finishing);
        if done {
            jobs::after(s, PROGRESS_HOLD, move |s, _| {
                if let Some(x) = side_of(s, &thread) {
                    x.browser.finishing = false;
                }
            });
        }
    });
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

/// One poll of loop `generation`, started on `thread`, at `now`: the wait
/// before the next, or `None` once it ends (another thread is open, a
/// newer loop runs, the tab hid, or the page is loaded and nobody has
/// touched it for [`WATCH_FOR`]).
fn step(
    s: &mut State,
    thread: &str,
    generation: jobs::Generation,
    now: Instant,
) -> Option<Duration> {
    let x = side_of(s, thread)?;
    if x.browser.poll != generation {
        return None;
    }
    if !tab_shown(s, Tab::Browser) {
        if let Some(x) = side_of(s, thread) {
            x.browser.polling = false;
        }
        return None;
    }
    let state = native_view::web_state();
    let x = side_of(s, thread)?;
    // The field follows the page unless the person is typing in it.
    if let Some(url) = state
        .as_ref()
        .map(|w| w.url.clone())
        .filter(|u| !u.is_empty())
        && !x.browser.input.focused
        && x.browser.input.text != url
    {
        x.browser.input.replace_text(url);
    }
    if state != x.browser.shown {
        let was_loading = x.browser.shown.as_ref().is_some_and(|w| w.loading);
        let loading_now = state.as_ref().is_some_and(|w| w.loading);
        if was_loading && !loading_now {
            x.browser.finishing = true;
        } else if loading_now {
            x.browser.finishing = false;
        }
        x.browser.shown = state;
    }
    let loading = x.browser.shown.as_ref().is_some_and(|w| w.loading);
    let watched = x.browser.watch_until.is_some_and(|until| now < until);
    if loading || watched {
        return Some(POLL_EVERY);
    }
    x.browser.watch_until = None;
    x.browser.polling = false;
    None
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
            .on_press(move |s: &mut State| command(s, cmd))
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

    fn loop_of(s: &State) -> (bool, jobs::Generation) {
        let x = side(s).expect("side state");
        (x.browser.polling, x.browser.poll)
    }

    #[test]
    fn a_poll_tick_only_runs_for_the_thread_and_loop_that_started_it() {
        let mut s = browsing("t1");
        start(&mut s);
        let (polling, first) = loop_of(&s);
        assert!(polling);
        assert!(
            step(&mut s, "t1", first, Instant::now()).is_some(),
            "its own loop goes on"
        );

        // Another thread opens and starts its own loop: the first one's
        // tick ends and leaves the new loop alone.
        s.chat = Some(crate::chat::ChatState::new("t2".into()));
        start(&mut s);
        let (_, second) = loop_of(&s);
        assert_ne!(first, second);
        assert!(step(&mut s, "t1", first, Instant::now()).is_none());
        assert!(
            step(&mut s, "t2", first, Instant::now()).is_none(),
            "an older loop on this thread ends too"
        );
        assert_eq!(loop_of(&s), (true, second), "the running loop is untouched");
        assert!(step(&mut s, "t2", second, Instant::now()).is_some());
    }

    /// The load bar follows the page's progress while it loads, shows full
    /// once the load ends, and goes when the hold clears.
    #[test]
    fn the_load_bar_follows_progress_then_shows_full_before_it_goes() {
        let mut s = browsing("t1");
        start(&mut s);
        let (_, generation) = loop_of(&s);
        assert_eq!(progress(&s), None, "no load seen yet: no bar");
        if let Some(x) = side_mut(&mut s) {
            x.browser.shown = Some(WebState {
                loading: true,
                progress: 0.4,
                ..WebState::default()
            });
        }
        assert_eq!(progress(&s), Some(0.4));
        assert!(progress_bar(&s, 300.).is_some());
        // The next poll finds the load over (a test has no web view).
        assert!(step(&mut s, "t1", generation, Instant::now()).is_some());
        assert_eq!(progress(&s), Some(1.), "done: full for a moment");
        if let Some(x) = side_mut(&mut s) {
            x.browser.finishing = false;
        }
        assert_eq!(progress(&s), None, "then it goes");
    }

    /// The energy point: a loaded page nobody touches is not polled. The
    /// loop runs out once the watch after the last action has passed, and
    /// asks for no further tick; input on the page starts it again.
    #[test]
    fn an_idle_loaded_page_is_not_polled() {
        let mut s = browsing("t1");
        start(&mut s);
        let (_, generation) = loop_of(&s);
        let later = Instant::now() + WATCH_FOR + Duration::from_millis(1);
        assert_eq!(
            step(&mut s, "t1", generation, later),
            None,
            "loaded, and the watch is over: no next tick"
        );
        assert_eq!(loop_of(&s).0, false, "the loop has ended");
        // A load in progress keeps it going however long it takes.
        page_input(&mut s);
        let (polling, generation) = loop_of(&s);
        assert!(polling, "a click in the page follows it again");
        if let Some(x) = side_mut(&mut s) {
            x.browser.shown = Some(WebState {
                loading: true,
                ..WebState::default()
            });
        }
        // (A test has no web view, so the poll sees the load end at once:
        // it still runs while the watch lasts.)
        assert_eq!(
            step(&mut s, "t1", generation, Instant::now()),
            Some(POLL_EVERY)
        );
    }

    /// Input on the page of a hidden Browser tab starts nothing.
    #[test]
    fn page_input_behind_another_tab_starts_no_poll() {
        let mut s = browsing("t1");
        s.prefs.side_tab = Tab::Activity.id().into();
        page_input(&mut s);
        assert!(!loop_of(&s).0);
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
