//! Settings → Permissions → Folders on this Mac: the folders ZLO was given
//! one at a time through macOS's open panel, instead of a whole Desktop,
//! Documents or Downloads switch (`folder_access.rs`).
//!
//! A change reaches a running backend only when it restarts, and a restart
//! stops the threads running now, so it is the person's to press (as for a
//! CLI install, `cli_install::restart_now`); otherwise it applies at the
//! next start.

use std::path::PathBuf;

use rune::{El, FileDialogOptions, Tx};
use rune_kit::Kit;

use crate::folder_access;
use crate::pages::common::{Btn, button_xs, group_header, rows_card, setting_row};
use crate::state::State;

pub fn view(s: &State, k: &Kit) -> El {
    let p = &s.palette;
    let mut rows: Vec<El> = folder_access::list()
        .into_iter()
        .enumerate()
        .map(|(ix, path)| {
            let shown = shown(&path);
            let remove = button_xs("Remove", Btn::Outline)
                .on_press(move |s: &mut State| forget(s, path.clone()))
                .build(k)
                .key(format!("folder-remove-{ix}"));
            setting_row(p, &shown, None, remove)
        })
        .collect();
    let add = button_xs("Add folder…", Btn::Primary)
        .on_press_tx(|_: &mut State, tx: &mut Tx| pick(tx))
        .build(k)
        .key("folder-add");
    if folder_access::pending() {
        rows.push(restart_row(s, k));
    }
    rows.push(setting_row(
        p,
        "Add a folder",
        Some(
            "Pick one folder in Desktop, Documents, Downloads, iCloud Drive or a drive. \
             ZLO and its agents can use only that folder, so you can turn down macOS's \
             whole-folder prompt.",
        ),
        add,
    ));
    rune::col()
        .w_full()
        .child(group_header(
            p,
            "Folders on this Mac",
            Some("What ZLO may read and change in folders macOS protects."),
        ))
        .child(rows_card(p, rows))
}

/// The running backend started before the last change: Restart IronClaw,
/// or leave it for the next start.
fn restart_row(s: &State, k: &Kit) -> El {
    let restarting = crate::cli_install::restarting(s);
    let mut restart = button_xs(
        if restarting {
            "Restarting…"
        } else {
            "Restart IronClaw"
        },
        Btn::Outline,
    )
    .disabled(restarting || !crate::cli_install::backend_on_this_mac(s));
    if !restarting {
        restart = restart.on_press(restart_now);
    }
    setting_row(
        &s.palette,
        "Restart to apply",
        Some(
            "IronClaw started before this change, so its agents do not have it yet. \
             Restart it now (threads running now stop), or it applies the next time ZLO starts.",
        ),
        restart.build(k).key("folder-restart"),
    )
}

fn restart_now(s: &mut State) {
    if crate::cli_install::restarting(s) || !crate::cli_install::backend_on_this_mac(s) {
        return;
    }
    if crate::service::restart() {
        crate::cli_install::note_restart(s);
        s.pages.settings.notice = Some("IronClaw is restarting with the new folder list.".into());
    } else {
        s.pages.settings.error =
            Some("This app did not start IronClaw, so restart it yourself to apply.".into());
    }
}

fn pick(tx: &mut Tx) {
    let options = FileDialogOptions {
        title: "Give ZLO this folder".into(),
        ..Default::default()
    };
    tx.open_folder(options, |s: &mut State, result, _| match result {
        Ok(Some(folder)) => match folder_access::add(&folder) {
            Ok(true) => s.pages.settings.notice = Some("Folder added.".into()),
            Ok(false) => {
                s.pages.settings.notice = Some("A folder you added already holds that one.".into())
            }
            Err(e) => s.pages.settings.error = Some(e),
        },
        Ok(None) => {}
        Err(e) => tracing::warn!(error = %e, "folder picker failed"),
    });
}

fn forget(s: &mut State, path: PathBuf) {
    if folder_access::remove(&path) {
        s.pages.settings.notice = Some(format!("ZLO no longer has {}.", shown(&path)));
    }
}

/// `~/Documents/site` rather than the full home path.
fn shown(path: &std::path::Path) -> String {
    match dirs::home_dir().and_then(|h| path.strip_prefix(h).ok().map(|r| r.to_path_buf())) {
        Some(rest) => format!("~/{}", rest.display()),
        None => path.display().to_string(),
    }
}
