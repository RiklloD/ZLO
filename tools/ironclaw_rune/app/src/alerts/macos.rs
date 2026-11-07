//! System notifications through `UNUserNotificationCenter`, for what GPUI's
//! `show_system_notification` and `on_system_notification_response` did.
//! Everything here runs on the main thread (Rune handlers and AppKit
//! delegate callbacks). Without an app bundle the notification center
//! refuses to start (it raises), so nothing is posted then.

use std::ffi::{CStr, CString, c_char};
use std::sync::OnceLock;

use block2::RcBlock;
use objc2::rc::Retained;
use objc2::runtime::{AnyClass, AnyObject, Bool, NSObject};
use objc2::{ClassType, define_class, msg_send};
use rune::AppHandle;

use crate::state::State;

#[link(name = "UserNotifications", kind = "framework")]
unsafe extern "C" {}

/// The app, for routing a click back to its state.
static APP: OnceLock<AppHandle<State>> = OnceLock::new();

/// UNAuthorizationOptionBadge | Sound | Alert.
const AUTH_OPTIONS: usize = 1 | 2 | 4;
/// UNNotificationPresentationOptionList | Banner | Sound: show while in front.
const PRESENT_OPTIONS: usize = (1 << 3) | (1 << 4) | (1 << 1);

define_class!(
    #[unsafe(super(NSObject))]
    #[name = "ZLORuneNotificationDelegate"]
    struct Delegate;

    impl Delegate {
        #[unsafe(method(userNotificationCenter:didReceiveNotificationResponse:withCompletionHandler:))]
        fn did_receive(
            &self,
            _center: *mut AnyObject,
            response: *mut AnyObject,
            handler: &block2::Block<dyn Fn()>,
        ) {
            if let Some(tag) = unsafe { response_tag(response) }
                && let Some(app) = APP.get()
            {
                app.open();
                app.update(move |s, _| crate::workspace::alert_clicked(s, &tag));
            }
            handler.call(());
        }

        #[unsafe(method(userNotificationCenter:willPresentNotification:withCompletionHandler:))]
        fn will_present(
            &self,
            _center: *mut AnyObject,
            _notification: *mut AnyObject,
            handler: &block2::Block<dyn Fn(usize)>,
        ) {
            handler.call((PRESENT_OPTIONS,));
        }
    }
);

/// `response.notification.request.identifier`.
unsafe fn response_tag(response: *mut AnyObject) -> Option<String> {
    if response.is_null() {
        return None;
    }
    unsafe {
        let notification: *mut AnyObject = msg_send![response, notification];
        if notification.is_null() {
            return None;
        }
        let request: *mut AnyObject = msg_send![notification, request];
        if request.is_null() {
            return None;
        }
        let id: *mut AnyObject = msg_send![request, identifier];
        string_of(id)
    }
}

unsafe fn string_of(ns: *mut AnyObject) -> Option<String> {
    if ns.is_null() {
        return None;
    }
    let utf8: *const c_char = unsafe { msg_send![ns, UTF8String] };
    if utf8.is_null() {
        return None;
    }
    Some(
        unsafe { CStr::from_ptr(utf8) }
            .to_string_lossy()
            .into_owned(),
    )
}

fn ns_string(s: &str) -> Option<*mut AnyObject> {
    let c = CString::new(s.replace('\0', " ")).ok()?;
    let class = AnyClass::get(c"NSString")?;
    let ns: *mut AnyObject = unsafe { msg_send![class, stringWithUTF8String: c.as_ptr()] };
    (!ns.is_null()).then_some(ns)
}

/// The notification center, when this process may use one (it runs from a
/// bundle); `None` with a debug line otherwise.
fn center() -> Option<*mut AnyObject> {
    let bundle_class = AnyClass::get(c"NSBundle")?;
    let bundle: *mut AnyObject = unsafe { msg_send![bundle_class, mainBundle] };
    let id: *mut AnyObject = if bundle.is_null() {
        std::ptr::null_mut()
    } else {
        unsafe { msg_send![bundle, bundleIdentifier] }
    };
    if id.is_null() {
        tracing::debug!("not running from an app bundle: system notifications are off");
        return None;
    }
    let class = AnyClass::get(c"UNUserNotificationCenter")?;
    let center: *mut AnyObject = unsafe { msg_send![class, currentNotificationCenter] };
    (!center.is_null()).then_some(center)
}

pub(super) fn install(handle: AppHandle<State>) {
    let _ = APP.set(handle);
    let Some(center) = center() else { return };
    // The center holds its delegate weakly: this one lives for the process.
    let delegate: Retained<Delegate> = unsafe { msg_send![Delegate::class(), new] };
    let delegate = Retained::into_raw(delegate);
    unsafe {
        let _: () = msg_send![center, setDelegate: delegate as *mut AnyObject];
    }
    let done = RcBlock::new(|granted: Bool, _error: *mut AnyObject| {
        if !granted.as_bool() {
            tracing::debug!("system notifications not allowed");
        }
    });
    unsafe {
        let _: () = msg_send![
            center,
            requestAuthorizationWithOptions: AUTH_OPTIONS,
            completionHandler: &*done
        ];
    }
}

pub(super) fn post(tag: &str, title: &str, body: &str) {
    let Some(center) = center() else { return };
    let (Some(content_class), Some(request_class)) = (
        AnyClass::get(c"UNMutableNotificationContent"),
        AnyClass::get(c"UNNotificationRequest"),
    ) else {
        return;
    };
    let (Some(tag), Some(title), Some(body)) = (ns_string(tag), ns_string(title), ns_string(body))
    else {
        return;
    };
    unsafe {
        let content: *mut AnyObject = msg_send![content_class, new];
        if content.is_null() {
            return;
        }
        let _: () = msg_send![content, setTitle: title];
        let _: () = msg_send![content, setBody: body];
        let request: *mut AnyObject = msg_send![
            request_class,
            requestWithIdentifier: tag,
            content: content,
            trigger: std::ptr::null_mut::<AnyObject>()
        ];
        let _: () = msg_send![content, release];
        if request.is_null() {
            return;
        }
        let done = RcBlock::new(|error: *mut AnyObject| {
            if !error.is_null() {
                tracing::debug!("system notification refused");
            }
        });
        let _: () = msg_send![
            center,
            addNotificationRequest: request,
            withCompletionHandler: &*done
        ];
    }
}
