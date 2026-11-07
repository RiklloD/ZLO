//! Settings → General → SnapShots: the switch, whether the window's text
//! comes along, and the fallback shortcut. Turning a part on asks macOS for
//! the permission it needs: Screen Recording for the picture, Accessibility
//! only for the text. Port of GPUI `pages/settings/snapshots.rs`.

use rune::{El, col};
use rune_kit::Kit;

use crate::pages::common::{group_header, rows_card, segmented, setting_row, switch};
use crate::snapshot::{self, FALLBACKS};
use crate::state::State;

pub fn view(s: &State, k: &Kit) -> El {
    let p = &s.palette;
    let prefs = &s.snapshot.prefs;
    let enabled = switch(k, prefs.enabled, false, "SnapShots", |s: &mut State| {
        let on = !s.snapshot.prefs.enabled;
        if on {
            ask(true, s.snapshot.prefs.include_text);
        }
        snapshot::update_prefs(s, |p| p.enabled = on);
    });
    let text = switch(
        k,
        prefs.include_text,
        !prefs.enabled,
        "Include app text",
        |s: &mut State| {
            let on = !s.snapshot.prefs.include_text;
            if on {
                ask(false, true);
            }
            snapshot::update_prefs(s, |p| p.include_text = on);
        },
    );
    let labels: Vec<&str> = FALLBACKS.iter().map(|(_, label)| *label).collect();
    let selected = FALLBACKS
        .iter()
        .position(|(key, _)| *key == prefs.fallback)
        .unwrap_or(0);
    let shortcut = segmented(
        k,
        &labels,
        selected,
        !prefs.enabled,
        |s: &mut State, ix, _| snapshot::update_prefs(s, |p| p.fallback = FALLBACKS[ix].0.into()),
    );
    let shortcut_note = snapshot::warning(s).unwrap_or_else(|| {
        "Also takes a SnapShot, for keyboards where both Shift keys are awkward.".into()
    });
    col()
        .w_full()
        .child(group_header(
            p,
            "SnapShots",
            Some("Press both Shift keys in any app to drop its front window into the chat draft."),
        ))
        .child(rows_card(
            p,
            [
                setting_row(
                    p,
                    "SnapShots",
                    Some(
                        "Captures the frontmost window of another app with its name and title. \
                         Needs Screen Recording.",
                    ),
                    enabled,
                ),
                setting_row(
                    p,
                    "Include app text",
                    Some(
                        "Adds the window's buttons, fields and text from its accessibility \
                         tree, so the agent can read the screen. Needs Accessibility.",
                    ),
                    text,
                ),
                setting_row(p, "Shortcut", Some(&shortcut_note), shortcut),
            ],
        ))
}

/// Ask macOS for what the switched-on parts need (GPUI `ask`).
fn ask(screen: bool, text: bool) {
    use crate::permissions::{self, Permission};
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
