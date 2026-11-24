//! The Projects step's view: the count, the repository groups that fold
//! open, the folder rows, and Import.

use std::collections::BTreeSet;

use rune::{El, col, el, row, text};
use rune_kit::{Checkbox, Kit, RollingText};
use zlo_client::session_import::SessionImportProject;

use crate::design::brands::provider_logo;
use crate::design::{kit as zkit, motion};
use crate::pages::common::controls::{Look, Size, button_sized, muted, spinner};
use crate::pages::common::ts::{MONO_XS, sm, sm_medium, xs};
use crate::state::State;

use super::super::view::{StepView, step_shell};
use super::{
    SCAN_LIMIT_MESSAGE, Scan, group, listed, run_import, scan, selected, set_keys, short_age, skip,
    split_path,
};

pub(super) fn view(s: &State, k: &Kit) -> StepView {
    let p = &s.palette;
    let ps = &s.onboarding.projects;
    let busy = ps.importing;
    match &ps.scan {
        None | Some(Scan::Pending) => {
            let body = col()
                .key("projects-loading")
                .w_full()
                .min_h(160.)
                .child(
                    text("Your projects")
                        .font(rune::TextStyle::new(22., 600.))
                        .color(p.text),
                )
                .child(
                    col()
                        .w_full()
                        .grow(1.)
                        .py(24.)
                        .gap(12.)
                        .center()
                        .child(spinner(k))
                        .child(
                            text("Looking for projects from Claude Code and Codex…")
                                .font(sm())
                                .color(p.text_muted),
                        ),
                );
            return StepView::new(body).action(skip_button(k, busy));
        }
        _ => {}
    }
    let mut body = step_shell(
        s,
        "Import your projects",
        "Import projects and conversations from Claude Code and Codex on your selected computer.",
    );
    let projects = listed(s);
    let pick = selected(s);
    if !projects.is_empty() {
        let all = pick.len() == projects.len();
        let all_keys: Vec<String> = projects.iter().map(|p| p.path.clone()).collect();
        body = body.child(
            row()
                .w_full()
                .pt(20.)
                .gap(12.)
                .center_y()
                .child(
                    // Rolls as the person picks (never on a clock).
                    RollingText::new(
                        "projects-count",
                        format!("{} of {} selected", pick.len(), projects.len()),
                    )
                    .value(pick.len() as f64)
                    .font(xs())
                    .color(p.text_muted)
                    .spring(motion::ROLL)
                    .build(k)
                    .grow(1.),
                )
                .child(
                    button_sized("Select all", Look::Ghost, Size::XSmall)
                        .disabled(busy || all)
                        .on_press(move |s: &mut State| set_keys(s, all_keys.clone(), true))
                        .build(k)
                        .key("projects-select-all")
                        .press_scale(motion::PRESS),
                )
                .child(
                    button_sized("Select none", Look::Ghost, Size::XSmall)
                        .disabled(busy || pick.is_empty())
                        .on_press(|s: &mut State| {
                            s.onboarding.projects.selection = Some(BTreeSet::new())
                        })
                        .build(k)
                        .key("projects-select-none")
                        .press_scale(motion::PRESS),
                ),
        );
    }
    let mut list = col().w_full().gap(2.);
    match &ps.scan {
        Some(Scan::Failed(e)) => {
            list = list.child(
                row()
                    .w_full()
                    .gap(12.)
                    .center_y()
                    .child(
                        text(format!("Could not check projects. {e}"))
                            .font(sm())
                            .color(p.text_muted)
                            .wrap()
                            .grow(1.),
                    )
                    .child(
                        button_sized("Retry", Look::Ghost, Size::Small)
                            .on_press(scan)
                            .build(k)
                            .key("projects-retry")
                            .press_scale(motion::PRESS),
                    ),
            );
        }
        Some(Scan::Unsupported) => {
            list = list.child(muted(
                p,
                "This IronClaw cannot list projects yet. Skip this step, or update IronClaw on that computer.",
            ));
        }
        Some(Scan::Ready(scan)) => {
            if projects.is_empty() {
                list = list.child(
                    text("No existing Claude Code or Codex projects found.")
                        .font(sm())
                        .color(p.text_muted)
                        .py(8.),
                );
            }
            if scan.truncated {
                list = list.child(muted(p, SCAN_LIMIT_MESSAGE));
            }
            list = list.child(candidate_list(s, k, projects, &pick));
        }
        _ => {}
    }
    // The card's body scrolls as one (`view::wizard`); no inner scroller.
    body = body.child(col().key("projects-list").w_full().pt(8.).child(list));
    if let Some(e) = &ps.error {
        body = body.child(
            text(e.clone())
                .key("projects-error")
                .font(sm())
                .color(p.danger)
                .pt(12.)
                .wrap()
                .transition(motion::reveal()),
        );
    }
    let n = pick.len();
    let import = {
        let label = if busy {
            "Importing…".to_string()
        } else {
            format!("Import {n} {}", if n == 1 { "project" } else { "projects" })
        };
        let roll = label.clone();
        let kit = k.clone();
        button_sized(label, Look::Primary, Size::Small)
            // The count rolls as the person picks; the button's width
            // springs with it.
            .label_with(move |font, ink| {
                RollingText::new("projects-import-label", roll)
                    .value(n as f64)
                    .font(font)
                    .color(ink)
                    .spring(motion::ROLL)
                    .build(&kit)
            })
            .disabled(busy || n == 0)
            .on_press(run_import)
            .build(k)
            .key("projects-import")
            .press_scale(motion::PRESS)
    };
    StepView::new(body)
        .action(skip_button(k, busy))
        .action(import)
}

fn skip_button(k: &Kit, busy: bool) -> El {
    button_sized("Do not import projects", Look::Ghost, Size::Small)
        .disabled(busy)
        .on_press(skip)
        .build(k)
        .key("projects-skip")
        .press_scale(motion::PRESS)
}

fn group_open(s: &State, key: &str, default: bool) -> bool {
    default != s.onboarding.projects.flipped.contains(key)
}

fn flip(s: &mut State, key: String) {
    let f = &mut s.onboarding.projects.flipped;
    if !f.remove(&key) {
        f.insert(key);
    }
}

fn candidate_list(
    s: &State,
    k: &Kit,
    projects: &[SessionImportProject],
    pick: &BTreeSet<String>,
) -> El {
    let (groups, other) = group(projects);
    let mut c = col().w_full().gap(2.);
    for g in &groups {
        if let [only] = g.candidates.as_slice() {
            c = c.child(candidate_row(s, k, only, Some(&g.label), false, pick));
            continue;
        }
        let keys: Vec<String> = g.candidates.iter().map(|p| p.path.clone()).collect();
        let open = group_open(s, &g.key, true);
        let sources: BTreeSet<&str> = g
            .candidates
            .iter()
            .flat_map(|p| p.sources.iter().map(String::as_str))
            .collect();
        c = c.child(group_header(
            s,
            k,
            &g.key,
            &g.label,
            keys,
            open,
            pick,
            Some(sources.into_iter().collect()),
            g.thread_count,
            g.last_active_at,
        ));
        if open {
            c = c.child(group_rows(
                &g.key,
                g.candidates
                    .iter()
                    .map(|p| candidate_row(s, k, p, None, true, pick)),
            ));
        }
    }
    if !other.is_empty() {
        let keys: Vec<String> = other.iter().map(|p| p.path.clone()).collect();
        let open = group_open(s, "other", false);
        let label = format!(
            "Other folders · {} {}",
            other.len(),
            if other.len() == 1 {
                "folder"
            } else {
                "folders"
            }
        );
        c = c.child(group_header(
            s, k, "other", &label, keys, open, pick, None, 0, None,
        ));
        if open {
            c = c.child(group_rows(
                "other",
                other
                    .iter()
                    .map(|p| candidate_row(s, k, p, None, true, pick)),
            ));
        }
    }
    c
}

/// An open group's folder rows, as one keyed block that wipes open under
/// its header and folds away faster. Its gap is the list's, so the rows sit
/// where they did as the list's own children.
fn group_rows(key: &str, rows: impl Iterator<Item = El>) -> El {
    col()
        .key(format!("group-rows-{key}"))
        .w_full()
        .gap(2.)
        .transition(motion::reveal())
        .children(rows)
}

// arch-exempt: too_many_args, one row's parts; a view-only helper, plan #onboarding-port
#[allow(clippy::too_many_arguments)]
fn group_header(
    s: &State,
    k: &Kit,
    key: &str,
    label: &str,
    keys: Vec<String>,
    open: bool,
    pick: &BTreeSet<String>,
    sources: Option<Vec<&str>>,
    threads: u32,
    last: Option<i64>,
) -> El {
    let p = &s.palette;
    let chosen = keys.iter().filter(|k| pick.contains(*k)).count();
    let all = chosen == keys.len();
    let busy = s.onboarding.projects.importing;
    let toggle_keys = keys.clone();
    let flip_key = key.to_string();
    let mut line = row()
        .key(format!("group-{key}"))
        .w_full()
        .gap(10.)
        .px(8.)
        .py(6.)
        .radius(6.)
        .center_y()
        .hover(p.hover)
        .hover_motion(motion::HOVER, motion::HOVER)
        .child(
            Checkbox::new(all)
                .indeterminate(chosen > 0 && !all)
                .label(label.to_string())
                .hide_label()
                .disabled(busy)
                .on_toggle(move |s: &mut State| set_keys(s, toggle_keys.clone(), !all))
                .build(&crate::design::kit::checkbox_kit(&s.palette, k))
                .key(format!("group-check-{key}")),
        )
        .child(
            row()
                .grow(1.)
                .min_w(0.)
                .gap(6.)
                .center_y()
                .cursor(rune::Cursor::Pointer)
                .press_scale(motion::PRESS_CARD)
                .on_press(move |s: &mut State| flip(s, flip_key.clone()))
                .child(zkit::chevron(
                    &format!("group-chevron-{key}"),
                    open,
                    zkit::Turn::Disclose,
                    14.,
                    p.text_muted,
                ))
                .child(
                    text(label.to_string())
                        .font(sm_medium())
                        .color(if sources.is_some() {
                            p.text
                        } else {
                            p.text_muted
                        })
                        .truncate()
                        .min_w(0.),
                ),
        );
    if let Some(sources) = sources {
        line = line.child(meta(s, Some(sources), threads, last));
    }
    line
}

fn candidate_row(
    s: &State,
    k: &Kit,
    project: &SessionImportProject,
    label: Option<&str>,
    nested: bool,
    pick: &BTreeSet<String>,
) -> El {
    let p = &s.palette;
    let on = pick.contains(&project.path);
    let path = project.path.clone();
    let (name, parent) = split_path(&project.path);
    let mut text_part = row().grow(1.).min_w(0.).gap(8.).center_y();
    if let Some(label) = label {
        text_part = text_part.child(
            text(label.to_string())
                .font(sm_medium())
                .color(p.text)
                .truncate()
                .min_w(0.),
        );
    }
    let path_ink = if label.is_some() {
        p.text_faint
    } else {
        p.text
    };
    text_part = text_part.child(
        row()
            .min_w(0.)
            .child(
                text(parent.to_string())
                    .font(MONO_XS)
                    .color(p.text_faint)
                    .truncate()
                    .min_w(0.),
            )
            .child(
                text(name.to_string())
                    .font(MONO_XS)
                    .color(path_ink)
                    .single_line(),
            ),
    );
    let sources: Vec<&str> = project.sources.iter().map(String::as_str).collect();
    row()
        .key(format!("project-{}", project.path))
        .w_full()
        .gap(10.)
        .pl(if nested { 32. } else { 8. })
        .pr(8.)
        .py(6.)
        .radius(6.)
        .center_y()
        .hover(p.hover)
        .hover_motion(motion::HOVER, motion::HOVER)
        .press_scale(motion::PRESS_CARD)
        .cursor(rune::Cursor::Pointer)
        .help(project.path.clone())
        .on_press(move |s: &mut State| {
            if !s.onboarding.projects.importing {
                set_keys(s, vec![path.clone()], !on)
            }
        })
        .child(
            Checkbox::new(on)
                .label(project.title.clone())
                .hide_label()
                .disabled(s.onboarding.projects.importing)
                .build(&crate::design::kit::checkbox_kit(&s.palette, k))
                .key(format!("project-check-{}", project.path)),
        )
        .child(text_part)
        .child(meta(
            s,
            (!nested).then_some(sources),
            project.thread_count,
            project.last_active_at,
        ))
}

/// t3code `ImportRowMeta`: source marks, the thread count and the age, in
/// fixed columns so rows line up.
fn meta(s: &State, sources: Option<Vec<&str>>, threads: u32, last: Option<i64>) -> El {
    let p = &s.palette;
    let now = chrono::Utc::now().timestamp_millis();
    let mut marks = row().w(40.).gap(4.).center_y();
    for (source, logo) in [("claude_code", "claude-cli"), ("codex", "codex")] {
        let shown = sources.as_ref().is_some_and(|list| list.contains(&source));
        marks = marks.child(if shown {
            el().w(16.).center().child(provider_logo(s, &[logo], 12.))
        } else {
            el().w(16.)
        });
    }
    row()
        .gap(4.)
        .center_y()
        .child(marks)
        .child(
            text(threads.to_string())
                .font(xs())
                .color(p.text_muted)
                .w(32.)
                .align(rune::Align::End),
        )
        .child(
            text(last.map(|at| short_age(at, now)).unwrap_or_default())
                .font(xs())
                .color(p.text_muted)
                .w(36.)
                .align(rune::Align::End),
        )
}
