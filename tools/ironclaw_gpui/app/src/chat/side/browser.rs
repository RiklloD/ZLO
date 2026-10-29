//! Browser: one web page beside the chat, for `localhost` previews, docs and
//! anything the agent links to. The page is a `WKWebView` from
//! `native_view`, created when this tab shows and gone soon after it hides.
//! There is one page for the whole app, so switching threads keeps it.

use std::cell::RefCell;
use std::time::Duration;

use super::{ChatView, Tab};
use crate::design::Palette;
use crate::native_view::{self, Slot, Source, WebCommand, WebState};
use crate::ui::*;

thread_local! {
    /// The address last asked for; the page may have moved on since.
    static PAGE: RefCell<Option<String>> = const { RefCell::new(None) };
}

fn page() -> Option<String> {
    PAGE.with(|p| p.borrow().clone())
}

#[derive(Default)]
pub(super) struct State {
    input: Option<(Entity<InputState>, Subscription)>,
    /// The page state the toolbar last drew.
    shown: Option<WebState>,
    polling: bool,
}

impl ChatView {
    /// Show `url` in the Browser tab.
    pub(super) fn browser_go(&mut self, url: String, cx: &mut Context<Self>) {
        let source = Source::Url(url.clone());
        PAGE.with(|p| *p.borrow_mut() = Some(url));
        native_view::load(Slot::Browser, &source);
        self.show_side(Tab::Browser, cx);
    }

    /// Close the page: its view goes now, the tab shows the empty address.
    fn browser_close(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        PAGE.with(|p| p.borrow_mut().take());
        native_view::close(Slot::Browser);
        self.side.browser.shown = None;
        if let Some((input, _)) = &self.side.browser.input {
            input.update(cx, |s, cx| s.set_value("", window, cx));
        }
        cx.notify();
    }

    /// Follow the page while the tab shows: quick while it loads, slow after,
    /// stopped once the tab hides.
    fn browser_poll(&mut self, cx: &mut Context<Self>) {
        if self.side.browser.polling {
            return;
        }
        self.side.browser.polling = true;
        cx.spawn(async move |this, cx| {
            let mut wait = Duration::from_millis(250);
            loop {
                cx.background_executor().timer(wait).await;
                let more = this.update(cx, |this, cx| {
                    if !this.tab_shown(Tab::Browser, cx) {
                        this.side.browser.polling = false;
                        return None;
                    }
                    let state = native_view::web_state();
                    let loading = state.as_ref().is_some_and(|s| s.loading);
                    if state != this.side.browser.shown {
                        this.side.browser.shown = state;
                        cx.notify();
                    }
                    Some(loading)
                });
                match more {
                    Ok(Some(loading)) => {
                        wait = Duration::from_millis(if loading { 250 } else { 1000 })
                    }
                    _ => break,
                }
            }
        })
        .detach();
    }

    fn browser_input(&mut self, window: &mut Window, cx: &mut Context<Self>) -> Entity<InputState> {
        if let Some((input, _)) = &self.side.browser.input {
            return input.clone();
        }
        let input =
            cx.new(|cx| InputState::new(window, cx).placeholder("Address, e.g. localhost:3000"));
        let sub = cx.subscribe_in(&input, window, |this, input, event, _, cx| {
            if let InputEvent::PressEnter { .. } = event {
                let text = input.read(cx).value().trim().to_string();
                if let Some(url) = address(&text) {
                    this.browser_go(url, cx);
                }
            }
        });
        self.side.browser.input = Some((input.clone(), sub));
        input
    }

    pub(super) fn render_browser(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        use gpui_kit::Focusable as _;
        let p = *Palette::of(cx);
        let input = self.browser_input(window, cx);
        let state = self.side.browser.shown.clone().unwrap_or_default();
        // The field follows the page unless the person is typing in it.
        if !state.url.is_empty()
            && !input.focus_handle(cx).is_focused(window)
            && input.read(cx).value() != state.url.as_str()
        {
            let url = state.url.clone();
            input.update(cx, |s, cx| s.set_value(url, window, cx));
        }
        let url = page();
        if url.is_some() {
            self.browser_poll(cx);
        }
        let nav =
            |id: &'static str, icon: IconName, tip: &'static str, on: bool, cmd: WebCommand| {
                Button::new(id)
                    .icon(icon)
                    .ghost()
                    .xsmall()
                    .disabled(!on)
                    .tooltip(tip)
                    .accessibility_label(tip)
                    .on_click(move |_, _, _| native_view::web(cmd))
            };
        let open_url = url.clone();
        let toolbar = h_flex()
            .px_2()
            .py_1()
            .gap_1()
            .items_center()
            .border_b_1()
            .border_color(p.border)
            .child(nav(
                "web-back",
                IconName::ArrowLeft,
                "Back",
                state.back,
                WebCommand::Back,
            ))
            .child(nav(
                "web-forward",
                IconName::ArrowRight,
                "Forward",
                state.forward,
                WebCommand::Forward,
            ))
            .child(if state.loading {
                nav("web-stop", IconName::Close, "Stop", true, WebCommand::Stop)
            } else {
                nav(
                    "web-reload",
                    IconName::RefreshCw,
                    "Reload",
                    open_url.is_some(),
                    WebCommand::Reload,
                )
            })
            .child(div().flex_1().min_w_0().child(Input::new(&input).small()))
            .child(
                Button::new("web-external")
                    .icon(IconName::ExternalLink)
                    .ghost()
                    .xsmall()
                    .disabled(open_url.is_none())
                    .tooltip("Open in your browser")
                    .accessibility_label("Open in your browser")
                    .on_click(move |_, _, cx| {
                        let current = native_view::web_state()
                            .map(|s| s.url)
                            .filter(|u| !u.is_empty())
                            .or_else(|| open_url.clone());
                        if let Some(url) = current {
                            cx.open_url(&url);
                        }
                    }),
            )
            .when(url.is_some(), |d| {
                d.child(
                    Button::new("web-close")
                        .icon(IconName::CircleX)
                        .ghost()
                        .xsmall()
                        .tooltip("Close page")
                        .accessibility_label("Close page")
                        .on_click(
                            cx.listener(|this, _, window, cx| this.browser_close(window, cx)),
                        ),
                )
            });
        let body = match url {
            Some(url) => div()
                .flex_1()
                .min_h_0()
                .child(native_view::surface(Slot::Browser, Source::Url(url)))
                .into_any_element(),
            None => v_flex()
                .flex_1()
                .items_center()
                .justify_center()
                .gap_2()
                .p_6()
                .child(Icon::new(IconName::Globe).size(px(28.)).text_color(p.text_faint))
                .child(
                    div()
                        .text_sm()
                        .text_center()
                        .text_color(p.text_muted)
                        .whitespace_normal()
                        .child("Type an address above. A local dev server works too, like localhost:3000."),
                )
                .into_any_element(),
        };
        v_flex()
            .flex_1()
            .min_h_0()
            .child(toolbar)
            .child(body)
            .into_any_element()
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
    use super::address;

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
