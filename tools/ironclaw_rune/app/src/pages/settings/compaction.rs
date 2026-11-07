//! "Compact idle Claude threads": the engine setting that has an idle Claude
//! CLI thread send `/compact` while its prompt cache is still warm. Each run
//! is listed with its cost on the Usage page. Port of GPUI
//! `pages/settings/compaction.rs`.

use rune::{El, col};
use rune_kit::Kit;

use crate::jobs;
use crate::pages::common::{error_line, rows_card, setting_row, switch};
use crate::state::State;

#[derive(Default)]
pub struct CompactionState {
    /// The stored value; `None` until the gateway answered.
    pub enabled: Option<bool>,
    pub saving: bool,
    pub error: Option<String>,
}

pub fn refresh(s: &mut State) {
    jobs::run(
        s,
        "compaction",
        |c| async move { c.idle_compact().await },
        |s, result, _| {
            let this = &mut s.pages.settings.compaction;
            match result {
                Ok(enabled) => {
                    this.enabled = Some(enabled);
                    this.error = None;
                }
                Err(e) => this.error = Some(e.to_string()),
            }
        },
    );
}

fn set(s: &mut State, enabled: bool) {
    let this = &mut s.pages.settings.compaction;
    if this.saving {
        return;
    }
    this.saving = true;
    jobs::run(
        s,
        "compaction",
        move |c| async move { c.set_idle_compact(enabled).await },
        |s, result, _| {
            let this = &mut s.pages.settings.compaction;
            this.saving = false;
            // Show the value the engine stored, not the click.
            match result {
                Ok(stored) => {
                    this.enabled = Some(stored);
                    this.error = None;
                }
                Err(e) => this.error = Some(e.to_string()),
            }
        },
    );
}

pub fn view(s: &State, k: &Kit) -> El {
    let p = &s.palette;
    let this = &s.pages.settings.compaction;
    let on = this.enabled.unwrap_or(false);
    let toggle = switch(
        k,
        on,
        this.enabled.is_none() || this.saving,
        "Compact idle Claude threads",
        move |s: &mut State| set(s, !on),
    );
    let mut d = col().w_full();
    if let Some(e) = &this.error {
        d = d.child(error_line(p, e));
    }
    d.child(rows_card(
        p,
        [setting_row(
            p,
            "Compact idle Claude threads",
            Some("After 4.5 idle minutes, a Claude CLI thread with a large context sends /compact while the prompt cache is still warm, so your next message costs less. Each run shows on the Usage page."),
            toggle,
        )],
    ))
}

pub fn fixture(s: &mut State) {
    s.pages.settings.compaction.enabled = Some(true);
}
