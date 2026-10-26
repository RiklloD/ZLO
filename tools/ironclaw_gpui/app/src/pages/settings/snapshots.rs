//! Settings → General → SnapShots: the switch, whether the window's text
//! comes along, and the fallback shortcut. Turning a part on asks macOS for
//! the permission it needs, through `crate::permissions`: Screen Recording
//! for the picture, Accessibility only for the text.

use super::SettingsView;
use crate::pages::{group_header, rows_card, setting_row};
use crate::permissions::{self, Permission};
use crate::snapshot::{self, FALLBACKS, SnapshotPrefs};
use crate::ui::*;

pub(super) fn render(cx: &Context<SettingsView>) -> impl IntoElement {
    let prefs = SnapshotPrefs::of(cx);
    let enabled = Switch::new("snapshots-on")
        .checked(prefs.enabled)
        .accessibility_label("SnapShots")
        .on_click(|checked: &bool, _, cx| {
            let on = *checked;
            if on {
                ask(true, SnapshotPrefs::of(cx).include_text);
            }
            SnapshotPrefs::update(cx, |p| p.enabled = on);
        });
    let text = Switch::new("snapshots-text")
        .checked(prefs.include_text)
        .disabled(!prefs.enabled)
        .accessibility_label("Include app text")
        .on_click(|checked: &bool, _, cx| {
            let on = *checked;
            if on {
                ask(false, true);
            }
            SnapshotPrefs::update(cx, |p| p.include_text = on);
        });
    let labels: Vec<&'static str> = FALLBACKS.iter().map(|(_, label)| *label).collect();
    let selected = FALLBACKS
        .iter()
        .position(|(key, _)| *key == prefs.fallback)
        .unwrap_or(0);
    let shortcut = crate::design::kit::segmented(
        "snapshot-shortcut",
        &labels,
        selected,
        !prefs.enabled,
        |ix, _, cx| SnapshotPrefs::update(cx, |p| p.fallback = FALLBACKS[ix].0.into()),
        cx,
    );
    let shortcut_note = snapshot::warning(cx).unwrap_or_else(|| {
        "Also takes a SnapShot, for keyboards where both Shift keys are awkward.".into()
    });
    v_flex()
        .child(group_header(
            "SnapShots",
            Some("Press both Shift keys in any app to drop its front window into the chat draft."),
            cx,
        ))
        .child(rows_card(
            [
                setting_row(
                    "SnapShots",
                    Some(
                        "Captures the frontmost window of another app with its name and title. \
                         Needs Screen Recording."
                            .into(),
                    ),
                    enabled,
                    cx,
                ),
                setting_row(
                    "Include app text",
                    Some(
                        "Adds the window's buttons, fields and text from its accessibility \
                         tree, so the agent can read the screen. Needs Accessibility."
                            .into(),
                    ),
                    text,
                    cx,
                ),
                setting_row("Shortcut", Some(shortcut_note.into()), shortcut, cx),
            ],
            cx,
        ))
}

/// Ask macOS for what SnapShots are missing: Screen Recording for the
/// picture, Accessibility for the text.
fn ask(screen: bool, text: bool) {
    let wanted: Vec<Permission> = permissions::missing()
        .into_iter()
        .filter(|p| {
            (screen && *p == Permission::ScreenRecording)
                || (text && *p == Permission::Accessibility)
        })
        .collect();
    if !wanted.is_empty() {
        permissions::request(&wanted);
    }
}
