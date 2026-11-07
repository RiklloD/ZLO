//! Operator → Extensions: deployment credentials extensions declare for the
//! operator (Slack bot token, Telegram bot, Google OAuth client). One form
//! per group; a blank secret keeps the stored value. GPUI
//! `pages/operator/extensions.rs`.

use crate::pages::common::controls::{Look, Size, button_sized, input, muted, outlined_tag};
use crate::pages::common::ts::{BODY_SEMIBOLD, XS, XS_MEDIUM};
use crate::pages::common::{Tone, card, empty_state, error_line, loading};
use ironclaw_gpui_client::ClientError;
use ironclaw_gpui_client::operator::AdminConfigGroup;
use rune::{El, TextEdit, col, row, text};
use rune_kit::Kit;

use crate::jobs;
use crate::state::State;

/// One group with an input per field, in field order.
pub struct GroupForm {
    pub group: AdminConfigGroup,
    pub inputs: Vec<TextEdit>,
    pub saving: bool,
    pub saved: bool,
}

#[derive(Default)]
pub struct ExtensionConfigPanel {
    pub groups: Vec<GroupForm>,
    pub loaded: bool,
    pub error: Option<String>,
    /// The accessor target for a field whose form was rebuilt meanwhile.
    pub(super) scratch: TextEdit,
}

fn st(s: &mut State) -> Option<&mut ExtensionConfigPanel> {
    s.pages.operator.extensions.as_mut()
}

fn field_edit(s: &mut State, g: usize, f: usize) -> &mut TextEdit {
    let o = &mut s.pages.operator;
    match o.extensions.as_mut() {
        Some(panel) => match panel.groups.get_mut(g).and_then(|x| x.inputs.get_mut(f)) {
            Some(edit) => edit,
            None => &mut panel.scratch,
        },
        None => &mut o.status_scratch,
    }
}

pub fn form(group: AdminConfigGroup) -> GroupForm {
    let inputs = group
        .fields
        .iter()
        .map(|f| match (f.secret, f.value.clone()) {
            (true, _) => TextEdit::new("").secure(),
            (false, Some(v)) => TextEdit::new(v),
            (false, None) => TextEdit::new(""),
        })
        .collect();
    GroupForm {
        group,
        inputs,
        saving: false,
        saved: false,
    }
}

pub fn refresh(s: &mut State) {
    jobs::run(
        s,
        "operator",
        |c| async move { c.extension_configuration().await },
        |s, result, _| {
            let Some(p) = st(s) else { return };
            match result {
                Ok(list) => {
                    p.groups = list.groups.into_iter().map(form).collect();
                    p.error = None;
                }
                Err(e) => p.error = Some(e.to_string()),
            }
            p.loaded = true;
        },
    );
}

fn save(s: &mut State, ix: usize) {
    let Some(g) = st(s).and_then(|p| p.groups.get_mut(ix)) else {
        return;
    };
    let values: Vec<(String, String)> = g
        .group
        .fields
        .iter()
        .zip(&g.inputs)
        .map(|(f, input)| (f.handle.clone(), input.text.trim().to_string()))
        .collect();
    // Clear typed secrets; the request owns them now.
    for (f, input) in g.group.fields.iter().zip(&mut g.inputs) {
        if f.secret {
            input.clear();
        }
    }
    g.saving = true;
    g.saved = false;
    let (id, revision) = (g.group.group_id.clone(), g.group.revision);
    jobs::run(
        s,
        "operator",
        move |c| async move {
            c.replace_extension_configuration(&id, revision, values)
                .await?;
            // Read back what the gateway stored.
            c.extension_configuration().await.map(|l| (id, l))
        },
        move |s, result, _| {
            let Some(p) = st(s) else { return };
            match result {
                Ok((id, list)) => {
                    p.groups = list
                        .groups
                        .into_iter()
                        .map(|group| {
                            let saved = group.group_id == id;
                            let mut f = form(group);
                            f.saved = saved;
                            f
                        })
                        .collect();
                    p.error = None;
                }
                Err(e) => {
                    if let Some(g) = p.groups.get_mut(ix) {
                        g.saving = false;
                    }
                    let conflict = matches!(e, ClientError::Api { status: 409, .. });
                    p.error = Some(if conflict {
                        "Someone saved this group in the meantime. Refresh and try again.".into()
                    } else {
                        e.to_string()
                    });
                }
            }
        },
    );
}

fn render_group(s: &State, k: &Kit, ix: usize, g: &GroupForm) -> El {
    let p = &s.palette;
    let used_by = g
        .group
        .used_by
        .iter()
        .map(|u| {
            if u.installed {
                format!("{} (installed)", u.display_name)
            } else {
                u.display_name.clone()
            }
        })
        .collect::<Vec<_>>()
        .join(", ");
    let mut c = card(p).key(format!("group-{}", g.group.group_id)).child(
        row()
            .w_full()
            .gap(8.)
            .center_y()
            .child(
                text(g.group.display_name.clone())
                    .font(BODY_SEMIBOLD)
                    .color(p.text)
                    .grow(1.),
            )
            .child(if g.group.complete {
                outlined_tag(p, Tone::Success, "Configured")
            } else {
                outlined_tag(p, Tone::Warning, "Needs values")
            }),
    );
    if !g.group.description.is_empty() {
        c = c.child(muted(p, g.group.description.clone()));
    }
    if !used_by.is_empty() {
        c = c.child(muted(p, format!("Used by: {used_by}")));
    }
    for (fi, (f, edit)) in g.group.fields.iter().zip(&g.inputs).enumerate() {
        let field = input(edit)
            .label(f.label.clone())
            .on_edit(move |s: &mut State| field_edit(s, ix, fi));
        let field = match (f.secret, f.provided) {
            (true, true) => field.secure().placeholder("Saved. Leave blank to keep it."),
            (true, false) => field.secure(),
            _ => field,
        };
        let mut block = col()
            .w_full()
            .gap(2.)
            .child(
                text(format!("{}{}", f.label, if f.required { " *" } else { "" }))
                    .font(XS_MEDIUM)
                    .color(p.text),
            )
            .child(field.build(k).key(format!("field-{ix}-{fi}")));
        if !f.description.is_empty() {
            block = block.child(
                text(f.description.clone())
                    .font(XS)
                    .color(p.text_muted)
                    .wrap(),
            );
        }
        c = c.child(block);
    }
    let mut actions = row().w_full().gap(8.).justify_end().center_y();
    if g.saved {
        actions = actions.child(muted(p, "Saved."));
    }
    c.child(
        actions.child(
            loading(
                button_sized("Save", Look::Primary, Size::Small)
                    .on_press(move |s: &mut State| save(s, ix)),
                g.saving,
            )
            .build(k)
            .key(format!("save-ext-config-{ix}")),
        ),
    )
}

pub fn view(s: &State, k: &Kit, panel: &ExtensionConfigPanel) -> El {
    let p = &s.palette;
    let mut c = col().w_full().gap(12.).child(muted(
        p,
        "Credentials extensions need from the operator before anyone can connect them. \
         Each person still connects their own account on the Extensions page.",
    ));
    if let Some(e) = &panel.error {
        c = c.child(error_line(p, e));
    }
    if panel.loaded && panel.groups.is_empty() {
        c = c.child(empty_state(p, "No extension declares deployment settings."));
    }
    c.children(
        panel
            .groups
            .iter()
            .enumerate()
            .map(|(ix, g)| render_group(s, k, ix, g)),
    )
}
