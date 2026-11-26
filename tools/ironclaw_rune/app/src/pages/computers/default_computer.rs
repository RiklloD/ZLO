//! Settings → Computers: "New threads start on" preference.
//!
//! Self-contained section hooked into `pages/computers.rs` with one line.
//! Values: the computer last used (default, `None` in `Prefs`), or one named
//! computer (`this-mac` or an `environment_id`). A named computer that was
//! removed falls back to "Computer last used".

use rune::{El, Tx, col};
use rune_kit::{Kit, Select, SelectAppearance};
use zlo_client::environments::THIS_MAC;

use crate::pages::common::{group_header, rows_card, setting_row};
use crate::remote;
use crate::state::State;

const LAST_USED_VALUE: &str = "";
const LAST_USED_LABEL: &str = "Computer last used";

/// Resolve `s.prefs.default_computer` against the currently configured
/// computers. Returns `None` for "Computer last used" (or when the named
/// computer was removed).
pub fn effective(s: &State) -> Option<(String, String)> {
    let key = s
        .prefs
        .default_computer
        .as_deref()
        .filter(|k| !k.is_empty())?;
    if key == THIS_MAC {
        let local_ok = !s.remote.app_only
            && (s.remote.registry.run_local_backend || s.onboarding.computer.key() == THIS_MAC);
        return local_ok.then(|| (THIS_MAC.to_string(), "This Mac".to_string()));
    }
    s.remote
        .registry
        .get(key)
        .map(|env| (env.environment_id.clone(), env.label.clone()))
}

/// Options for the select: `(pref_value, display_label)`.
fn options(s: &State) -> Vec<(String, String)> {
    let mut out = vec![(LAST_USED_VALUE.into(), LAST_USED_LABEL.into())];
    if !s.remote.app_only
        && (s.remote.registry.run_local_backend || s.onboarding.computer.key() == THIS_MAC)
    {
        out.push((THIS_MAC.into(), "This Mac".into()));
    }
    for env in remote::saved(s) {
        out.push((env.environment_id.clone(), env.label.clone()));
    }
    out
}

/// The "New threads" section on Settings → Computers, shown when more than one
/// computer is configured.
pub fn section(s: &State, k: &Kit, layer: &mut Vec<El>) -> Option<El> {
    let opts = options(s);
    // Only meaningful when at least two computers exist (Last used + 2+).
    if opts.len() <= 2 {
        return None;
    }
    let p = &s.palette;
    let current_value = effective(s)
        .map(|(k, _)| k)
        .unwrap_or_else(|| LAST_USED_VALUE.into());
    let current_label = opts
        .iter()
        .find(|(v, _)| v == &current_value)
        .map(|(_, l)| l.clone())
        .unwrap_or_else(|| LAST_USED_LABEL.into());
    let labels: Vec<String> = opts.iter().map(|(_, l)| l.clone()).collect();
    let open = s.workspace.default_computer_menu;

    let control = Select::new("select-default-computer", &labels)
        .width(200.)
        .appearance(SelectAppearance::Field)
        .current(Some(&current_label))
        .label("New threads start on")
        .open(open)
        .on_toggle(|s: &mut State| {
            s.workspace.default_computer_menu = !s.workspace.default_computer_menu;
        })
        .on_close(|s: &mut State| s.workspace.default_computer_menu = false)
        .on_choose(move |s: &mut State, chosen: &str, _tx: &mut Tx| {
            s.workspace.default_computer_menu = false;
            let picked = opts
                .iter()
                .find(|(_, l)| l == chosen)
                .map(|(v, _)| v.clone())
                .filter(|v| !v.is_empty());
            s.update_prefs(|p| p.default_computer = picked);
        })
        .build(k, layer);

    Some(
        col()
            .key("default-computer-section")
            .w_full()
            .child(group_header(p, "New threads", None))
            .child(rows_card(
                p,
                [setting_row(
                    p,
                    "New threads start on",
                    Some("Opens new drafts on that computer when it is connected. Otherwise the draft stays where it is."),
                    control,
                )],
            )),
    )
}
