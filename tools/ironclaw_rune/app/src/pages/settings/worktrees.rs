//! Settings → General → Worktrees: whether new threads of a git-folder
//! profile start in their own worktree (off by default: they work in the
//! profile's folder), and removing the thread worktrees that hold no work.

use rune::{El, col};
use rune_kit::Kit;

use crate::jobs;
use crate::pages::common::{Btn, button_xs, group_header, rows_card, setting_row, switch};
use crate::state::State;

pub fn view(s: &State, k: &Kit) -> El {
    let p = &s.palette;
    let own = switch(
        k,
        s.prefs.thread_worktrees,
        false,
        "New threads in their own worktree",
        |s: &mut State| s.update_prefs(|p| p.thread_worktrees = !p.thread_worktrees),
    );
    let clean = button_xs("Remove empty worktrees", Btn::Outline)
        .disabled(s.pages.settings.cleaning_worktrees)
        .on_press(clean_empty)
        .build(k)
        .key("worktrees-clean");
    let card = rows_card(
        p,
        [
            setting_row(
                p,
                "New threads in their own worktree",
                Some(
                    "For profiles whose folder is a git checkout: each new thread gets its own \
                     worktree and branch, made from main. Off: threads work in the profile's \
                     folder. The new-thread screen can still switch either way.",
                ),
                own.key("worktrees-own"),
            ),
            setting_row(
                p,
                "Clean up",
                Some(
                    "Removes thread worktrees and branches with no changes and no commits. \
                     Deleting a thread does this for its worktree; ones with work are kept.",
                ),
                clean,
            ),
        ],
    );
    col()
        .w_full()
        .child(group_header(p, "Worktrees", None))
        .child(card)
}

/// Clean every listed thread's worktree; the server keeps any that hold
/// work or whose thread is running.
fn clean_empty(s: &mut State) {
    let threads: Vec<String> = s
        .workspace
        .inbox
        .list
        .rows
        .iter()
        .filter(|r| r.worktree_branch.is_some())
        .map(|r| r.thread_id.clone())
        .collect();
    s.pages.settings.cleaning_worktrees = true;
    s.pages.settings.notice = None;
    jobs::run(
        s,
        "worktrees",
        move |c| async move {
            let mut removed = 0usize;
            let mut kept = 0usize;
            for id in &threads {
                match c.clean_thread_worktree(id).await {
                    Ok(true) => removed += 1,
                    Ok(false) => kept += 1,
                    Err(e) => {
                        tracing::debug!(error = %e, thread = %id, "worktree not cleaned");
                        kept += 1;
                    }
                }
            }
            Ok::<_, zlo_client::ClientError>((removed, kept))
        },
        |s, result, tx| {
            tx.animated();
            s.pages.settings.cleaning_worktrees = false;
            s.pages.settings.notice = Some(match result {
                Ok((0, 0)) => "No thread worktrees to clean.".into(),
                Ok((removed, kept)) => format!(
                    "Removed {removed} empty worktree(s); kept {kept} with work or running."
                ),
                Err(e) => e.to_string(),
            });
            // The rows' branch labels catch up.
            crate::workspace::thread_updated(s);
        },
    );
}
