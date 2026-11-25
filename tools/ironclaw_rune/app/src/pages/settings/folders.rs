//! Settings → Permissions → Folders on this Mac: the folders ZLO was given
//! one at a time through macOS's open panel, instead of a whole Desktop,
//! Documents or Downloads switch (`folder_access.rs`).

use std::path::PathBuf;

use rune::{El, FileDialogOptions, Tx};
use rune_kit::Kit;

use crate::folder_access;
use crate::pages::common::{Btn, button_xs, group_header, rows_card, setting_row};
use crate::service;
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

fn pick(tx: &mut Tx) {
    let options = FileDialogOptions {
        title: "Give ZLO this folder".into(),
        ..Default::default()
    };
    tx.open_folder(options, |s: &mut State, result, _| match result {
        Ok(Some(folder)) => match folder_access::add(&folder) {
            Ok(true) => {
                let note = if service::restart() {
                    "Folder added. The backend restarted to pick it up."
                } else {
                    "Folder added."
                };
                s.pages.settings.notice = Some(note.into());
            }
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
        service::restart();
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
