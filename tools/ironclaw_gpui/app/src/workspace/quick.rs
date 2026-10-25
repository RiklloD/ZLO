//! The quick composer, after MonoCode's `quick_composer.rs` and
//! `quick_composer/hotkey.rs` (MIT, see `THIRD_PARTY_NOTICES.md`): ⇧⌘Space
//! from any app opens a small floating prompt. Enter sends it as a new
//! thread through the new-thread screen's own path (last-used profile, the
//! picked model) and brings the main window forward on that thread. Escape,
//! the shortcut again, or clicking elsewhere closes it. The global shortcut
//! goes through HIToolbox, so it needs no Accessibility permission.

use gpui_kit::{
    AnyWindowHandle, Bounds, EventEmitter, Focusable as _, Render, WindowBackgroundAppearance,
    WindowBounds, WindowKind, WindowOptions, point, size,
};

use super::Workspace;
use crate::design::{self, Palette, Style, kit, motion};
use crate::ui::*;

pub(super) const SHORTCUT: &str = "cmd-shift-space";
const WIDTH: f32 = 640.;
const HEIGHT: f32 = 132.;

#[derive(Default)]
pub(super) struct Quick {
    #[cfg(target_os = "macos")]
    _hotkey: Option<hotkey::Registration>,
    window: Option<AnyWindowHandle>,
    _listen: Option<Task<()>>,
    _events: Option<Subscription>,
}

pub(super) enum QuickEvent {
    Send(String),
    Close,
}

pub(super) struct QuickComposer {
    input: Entity<TextareaState>,
    /// Became the key window once; only then does losing focus close it.
    seen_active: bool,
    _subs: Vec<Subscription>,
}

impl EventEmitter<QuickEvent> for QuickComposer {}

impl QuickComposer {
    fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let input = cx.new(|cx| {
            TextareaState::new(window, cx)
                .auto_grow(1, 4)
                .placeholder("Ask IronClaw anything…")
                .submit_on_enter(true)
        });
        let subs = vec![
            cx.subscribe(&input, |this, _, event, cx| {
                if let InputEvent::PressEnter { shift: false, .. } = event {
                    let text = this.input.read(cx).value().trim().to_string();
                    if !text.is_empty() {
                        cx.emit(QuickEvent::Send(text));
                    }
                }
            }),
            // Clicking another app or window closes it, like Spotlight.
            cx.observe_window_activation(window, |this, window, cx| {
                if window.is_window_active() {
                    this.seen_active = true;
                } else if this.seen_active {
                    cx.emit(QuickEvent::Close);
                }
            }),
        ];
        Self {
            input,
            seen_active: false,
            _subs: subs,
        }
    }
}

impl Render for QuickComposer {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        motion::pump(window);
        let p = *Palette::of(cx);
        let s = *Style::of(cx);
        let t = motion::reveal(
            "quick-in",
            motion::MotionSpec::new(200, motion::EASE_OUT_EXPO),
            0,
            cx.reduce_motion(),
        );
        let card = kit::floating(&s, &p).apply(
            v_flex()
                .size_full()
                .rounded(px(16.))
                .px_4()
                .pt_3()
                .pb_2()
                .gap_2()
                .child(
                    div()
                        .flex_1()
                        .text_size(px(16.))
                        .child(Textarea::new(&self.input).appearance(false)),
                )
                .child(
                    h_flex()
                        .gap_3()
                        .items_center()
                        .text_xs()
                        .text_color(p.text_faint)
                        .child(div().size(px(6.)).rounded_full().bg(p.accent))
                        .child(div().flex_1().child("New thread in IronClaw"))
                        .child("↵ send")
                        .child("esc close"),
                ),
        );
        div()
            .size_full()
            .p(px(8.))
            .font_family(design::FONT_SANS)
            .text_color(p.text)
            .opacity(t)
            .top(px(4. * (1. - t)))
            .relative()
            .on_key_down(cx.listener(|_, e: &KeyDownEvent, _, cx| {
                if e.keystroke.key == "escape" {
                    cx.emit(QuickEvent::Close);
                }
            }))
            .child(card)
            .child(motion::frame_tail())
    }
}

impl Workspace {
    /// Register the global shortcut once at startup. A taken shortcut only
    /// logs: the app works the same without it.
    pub(super) fn start_quick_composer(&mut self, cx: &mut Context<Self>) {
        #[cfg(target_os = "macos")]
        match hotkey::Registration::new(SHORTCUT) {
            Ok((registration, mut presses)) => {
                self.quick._hotkey = Some(registration);
                self.quick._listen = Some(cx.spawn(async move |this, cx| {
                    use futures::StreamExt as _;
                    while presses.next().await.is_some() {
                        if this.update(cx, |this, cx| this.toggle_quick(cx)).is_err() {
                            break;
                        }
                    }
                }));
            }
            Err(e) => tracing::warn!("quick composer shortcut unavailable: {e}"),
        }
        #[cfg(not(target_os = "macos"))]
        let _ = cx;
    }

    fn close_quick(&mut self, cx: &mut App) {
        self.quick._events = None;
        if let Some(handle) = self.quick.window.take() {
            let _ = handle.update(cx, |_, window, _| window.remove_window());
        }
    }

    fn toggle_quick(&mut self, cx: &mut Context<Self>) {
        if self.quick.window.is_some() {
            self.close_quick(cx);
            return;
        }
        let Some(display) = cx.primary_display() else {
            return;
        };
        // Spotlight's spot: centred, a fifth of the way down the screen.
        let screen = display.bounds();
        let origin = point(
            screen.origin.x + (screen.size.width - px(WIDTH)) / 2.,
            screen.origin.y + screen.size.height / 5.,
        );
        let options = WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(Bounds::new(
                origin,
                size(px(WIDTH), px(HEIGHT)),
            ))),
            kind: WindowKind::PopUp,
            titlebar: None,
            is_resizable: false,
            is_movable: true,
            window_background: WindowBackgroundAppearance::Transparent,
            ..Default::default()
        };
        motion::restart("quick-in");
        cx.activate(true);
        // Built by hand rather than `gpui_kit::open_window`: its root paints
        // the theme background, and this window is a floating card.
        let mut built = None;
        let opened = cx.open_window(options, |window, cx| {
            let view = cx.new(|cx| QuickComposer::new(window, cx));
            let focus = view.read(cx).input.focus_handle(cx);
            window.focus(&focus, cx);
            window.activate_window();
            built = Some(view.clone());
            cx.new(|cx| {
                gpui_kit::base::Root::new(view, window, cx).bg(gpui_kit::transparent_black())
            })
        });
        match (opened, built) {
            (Ok(handle), Some(view)) => {
                self.quick.window = Some(handle.into());
                self.quick._events = Some(cx.subscribe(&view, |this, _, event, cx| match event {
                    QuickEvent::Close => this.close_quick(cx),
                    QuickEvent::Send(text) => {
                        let text = text.clone();
                        this.close_quick(cx);
                        let main = this.main_window;
                        cx.spawn(async move |this, cx| {
                            let _ = main.update(cx, |_, window, cx| {
                                window.activate_window();
                                let _ =
                                    this.update(cx, |this, cx| this.quick_send(text, window, cx));
                            });
                        })
                        .detach();
                    }
                }));
            }
            (Err(e), _) => tracing::warn!("could not open the quick composer: {e}"),
            (Ok(_), None) => {}
        }
    }
}

#[cfg(target_os = "macos")]
mod hotkey {
    //! A global shortcut through HIToolbox's `RegisterEventHotKey`.
    use std::ffi::c_void;

    use futures::channel::mpsc::{UnboundedReceiver, UnboundedSender, unbounded};

    #[repr(C)]
    struct EventType {
        class: u32,
        kind: u32,
    }

    #[repr(C)]
    struct HotKeyId {
        signature: u32,
        id: u32,
    }

    type Handler = unsafe extern "C" fn(*mut c_void, *mut c_void, *mut c_void) -> i32;

    #[link(name = "Carbon", kind = "framework")]
    unsafe extern "C" {
        fn GetApplicationEventTarget() -> *mut c_void;
        fn InstallEventHandler(
            target: *mut c_void,
            handler: Handler,
            count: u32,
            types: *const EventType,
            data: *mut c_void,
            reference: *mut *mut c_void,
        ) -> i32;
        fn RemoveEventHandler(reference: *mut c_void) -> i32;
        fn RegisterEventHotKey(
            code: u32,
            modifiers: u32,
            id: HotKeyId,
            target: *mut c_void,
            options: u32,
            reference: *mut *mut c_void,
        ) -> i32;
        fn UnregisterEventHotKey(reference: *mut c_void) -> i32;
    }

    pub(crate) struct Registration {
        hotkey: *mut c_void,
        handler: *mut c_void,
        // Boxed so its address stays put while HIToolbox holds it.
        _sender: Box<UnboundedSender<()>>,
    }

    unsafe extern "C" fn pressed(
        _next: *mut c_void,
        _event: *mut c_void,
        data: *mut c_void,
    ) -> i32 {
        // SAFETY: `data` is the boxed sender, alive until the handler is
        // removed in `Drop`, on the same main thread.
        let sender = unsafe { &*data.cast::<UnboundedSender<()>>() };
        let _ = sender.unbounded_send(());
        0
    }

    impl Registration {
        pub(crate) fn new(shortcut: &str) -> Result<(Self, UnboundedReceiver<()>), String> {
            let (code, modifiers) = parse(shortcut)?;
            let (tx, rx) = unbounded();
            let mut sender = Box::new(tx);
            let mut handler = std::ptr::null_mut();
            let mut hotkey = std::ptr::null_mut();
            // SAFETY: plain HIToolbox calls on the main thread with valid
            // out-pointers; the sender box outlives the handler (see Drop).
            unsafe {
                let target = GetApplicationEventTarget();
                let event = EventType {
                    class: u32::from_be_bytes(*b"keyb"),
                    // kEventHotKeyPressed
                    kind: 5,
                };
                let error = InstallEventHandler(
                    target,
                    pressed,
                    1,
                    &event,
                    (&mut *sender as *mut UnboundedSender<()>).cast(),
                    &mut handler,
                );
                if error != 0 {
                    return Err(format!("InstallEventHandler failed ({error})"));
                }
                let id = HotKeyId {
                    signature: u32::from_be_bytes(*b"IrCl"),
                    id: 1,
                };
                let error = RegisterEventHotKey(code, modifiers, id, target, 0, &mut hotkey);
                if error != 0 {
                    RemoveEventHandler(handler);
                    return Err(format!("{shortcut} is taken by another app ({error})"));
                }
            }
            Ok((
                Self {
                    hotkey,
                    handler,
                    _sender: sender,
                },
                rx,
            ))
        }
    }

    impl Drop for Registration {
        fn drop(&mut self) {
            // SAFETY: both references came from successful registrations.
            unsafe {
                UnregisterEventHotKey(self.hotkey);
                RemoveEventHandler(self.handler);
            }
        }
    }

    /// `cmd-shift-space` style: Carbon modifier bits and a virtual key code.
    fn parse(shortcut: &str) -> Result<(u32, u32), String> {
        let mut modifiers = 0;
        let mut key = None;
        for part in shortcut.split('-') {
            match part {
                "cmd" => modifiers |= 256,
                "shift" => modifiers |= 512,
                "alt" => modifiers |= 2048,
                "ctrl" => modifiers |= 4096,
                k => key = Some(k),
            }
        }
        let code = match key.ok_or("no key")? {
            "space" => 49,
            "i" => 34,
            "k" => 40,
            "j" => 38,
            other => return Err(format!("unsupported key {other}")),
        };
        if modifiers == 0 {
            return Err("a global shortcut needs a modifier".into());
        }
        Ok((code, modifiers))
    }
}
