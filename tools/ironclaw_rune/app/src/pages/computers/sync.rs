//! Settings → Computers: the settings-sync line under a saved computer
//! (`remote::config_sync`): what the last pass came to, "Sync now", and
//! the computer's switch.

use rune::{El, row};
use rune_kit::Kit;

use crate::pages::common::controls::{press_button, styled};
use crate::pages::common::ts::xs;
use crate::pages::common::{Btn, switch};
use crate::remote::config_sync::{self, Tone as SyncTone};
use crate::state::State;

/// Whether This Mac's settings reach this computer, under its row.
pub(super) fn line(s: &State, k: &Kit, key: &str, label: &str) -> Option<El> {
    let p = &s.palette;
    let line = config_sync::line(s, key, label)?;
    let ink = match line.tone {
        SyncTone::Quiet => p.text_muted,
        SyncTone::Good => p.success,
        SyncTone::Warn => p.warning,
        SyncTone::Bad => p.danger,
    };
    let mut out = row()
        .w_full()
        .gap(10.)
        .center_y()
        .px(16.)
        .pb(12.)
        .key(format!("sync-{key}"))
        .child(styled(line.text, xs(), ink).wrap().grow(1.).min_w(0.));
    if line.can_sync {
        let target = key.to_string();
        out = out.child(press_button(
            k,
            &format!("sync-now-{key}"),
            "Sync now",
            Btn::Ghost,
            None,
            false,
            move |s| config_sync::sync_now(s, &target),
        ));
    }
    let target = key.to_string();
    let on = line.on;
    Some(out.child(switch(
        k,
        on,
        false,
        &format!("Sync settings from This Mac to {label}"),
        move |s: &mut State| config_sync::set_enabled(s, &target, !on),
    )))
}
