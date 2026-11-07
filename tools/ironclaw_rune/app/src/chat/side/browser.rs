//! Browser: one web page beside the chat, for `localhost` previews, docs and
//! anything the agent links to (GPUI `chat/side/browser.rs`). The page is a
//! `WKWebView` from `native_view`, created when this tab shows and gone
//! soon after it hides. There is one page for the whole app, so switching
//! threads keeps it.

use std::cell::RefCell;
use std::time::Duration;

use rune::{El, TextEdit, col, icon, row};
use rune_kit::{Kit, TextInput};

use super::ui::{self, sm};
use super::{Tab, side, side_mut, side_of, side_total, tab_shown, thread_id};
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

#[derive(Default)]
pub(super) struct BrowserState {
    input: TextEdit,
    /// The page state the toolbar last drew.
    shown: Option<WebState>,
    polling: bool,
    /// The running poll loop; a tick of an older loop (or of another
    /// thread's side state) ends instead of doubling this one.
    poll: jobs::Generation,
}

/// Show `url` in the Browser tab.
pub(super) fn go(s: &mut State, url: String) {
    let source = Source::Url(url.clone());
    PAGE.with(|p| *p.borrow_mut() = Some(url));
    native_view::load(Slot::Browser, &source);
    super::show(s, Tab::Browser);
    start(s);
}

/// Close the page: its view goes now, the tab shows the empty address.
fn close(s: &mut State) {
    PAGE.with(|p| p.borrow_mut().take());
    native_view::close(Slot::Browser);
    if let Some(x) = side_mut(s) {
        x.browser.shown = None;
        x.browser.input.replace_text("");
    }
}

/// Follow the page while the tab shows: quick while it loads, slow after,
/// stopped once the tab hides. The loop belongs to the thread that started
/// it, under a fresh generation.
pub(super) fn start(s: &mut State) {
    if page().is_none() {
        return;
    }
    let Some(thread) = thread_id(s) else { return };
    let Some(x) = side_mut(s) else { return };
    if x.browser.polling {
        return;
    }
    x.browser.polling = true;
    let generation = x.browser.poll.bump();
    tick(s, thread, generation, Duration::from_millis(250));
}

fn tick(s: &State, thread: String, generation: jobs::Generation, wait: Duration) {
    jobs::after(s, wait, move |s, _| {
        if let Some(wait) = step(s, &thread, generation) {
            tick(s, thread, generation, wait);
        }
    });
}

/// One poll of loop `generation`, started on `thread`: the wait before the
/// next, or `None` once it ends (another thread is open, a newer loop runs,
/// or the tab hid).
fn step(s: &mut State, thread: &str, generation: jobs::Generation) -> Option<Duration> {
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
    let loading = state.as_ref().is_some_and(|s| s.loading);
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
        x.browser.shown = state;
    }
    Some(Duration::from_millis(if loading { 250 } else { 1000 }))
}

pub(super) fn view(s: &State, k: &Kit) -> El {
    let p = &s.palette;
    let Some(x) = side(s) else {
        return ui::loading(k);
    };
    let state = x.browser.shown.clone().unwrap_or_default();
    let url = page();
    let nav = |key: &str, i: &'static rune::Icon, tip: &str, on: bool, cmd: WebCommand| {
        ui::ibtn(i, tip)
            .disabled(!on)
            .on_press(move |_: &mut State| native_view::web(cmd))
            .build(k)
            .key(key.to_string())
    };
    let open_url = url.clone();
    let mut toolbar = row()
        .w_full()
        .px(8.)
        .py(4.)
        .gap(4.)
        .items_center()
        .child(nav(
            "web-back",
            &icons::ARROW_LEFT,
            "Back",
            state.back,
            WebCommand::Back,
        ))
        .child(nav(
            "web-forward",
            &icons::ARROW_RIGHT,
            "Forward",
            state.forward,
            WebCommand::Forward,
        ))
        .child(if state.loading {
            nav("web-stop", &icons::CLOSE, "Stop", true, WebCommand::Stop)
        } else {
            nav(
                "web-reload",
                &icons::REFRESH_CW,
                "Reload",
                open_url.is_some(),
                WebCommand::Reload,
            )
        })
        .child(
            row().grow(1.).min_w(0.).child(
                TextInput::new(&x.browser.input)
                    .width(None)
                    .placeholder("Address, e.g. localhost:3000")
                    .label("Address")
                    .on_edit(|s: &mut State| &mut side_total(s).browser.input)
                    .on_submit(|s: &mut State, _| {
                        let text = side_total(s).browser.input.text.trim().to_string();
                        if let Some(url) = address(&text) {
                            go(s, url);
                        }
                    })
                    .build(k)
                    .key("web-address"),
            ),
        )
        .child(
            ui::ibtn(&icons::EXTERNAL_LINK, "Open in your browser")
                .disabled(open_url.is_none())
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
                .key("web-external"),
        );
    if url.is_some() {
        toolbar = toolbar.child(
            ui::ibtn(&icons::CIRCLE_X, "Close page")
                .on_press(close)
                .build(k)
                .key("web-close"),
        );
    }
    let body = match url {
        Some(_) => native_view::surface(Slot::Browser),
        None => ui::fill_col()
            .items_center()
            .justify_center()
            .gap(8.)
            .padding(24.)
            .child(icon(&icons::GLOBE, 28.).color(p.text_faint))
            .child(ui::para(
                "Type an address above. A local dev server works too, like localhost:3000.",
                sm(),
                p.text_muted,
            )),
    };
    ui::fill_col()
        .child(toolbar)
        .child(col().w_full().h(1.).fill(p.border))
        .child(body)
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
        assert!(step(&mut s, "t1", first).is_some(), "its own loop goes on");

        // Another thread opens and starts its own loop: the first one's
        // tick ends and leaves the new loop alone.
        s.chat = Some(crate::chat::ChatState::new("t2".into()));
        start(&mut s);
        let (_, second) = loop_of(&s);
        assert_ne!(first, second);
        assert!(step(&mut s, "t1", first).is_none());
        assert!(
            step(&mut s, "t2", first).is_none(),
            "an older loop on this thread ends too"
        );
        assert_eq!(loop_of(&s), (true, second), "the running loop is untouched");
        assert!(step(&mut s, "t2", second).is_some());
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
}
