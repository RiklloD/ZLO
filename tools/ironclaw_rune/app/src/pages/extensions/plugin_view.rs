//! Drawing a plugin's consent sheet, its held-state note on the Plugins
//! list, and its own page (settings from its schema, then its
//! `settings.page` slots).

use rune::{El, col, el, icon, row, text};
use rune_kit::{Dialog, Kit, RadioItem, TextInput};
use serde_json::Value;
use zlo_client::plugins::{PluginSetting, PluginState, RequestKind, SettingKind, SlotKind};
use zlo_client::wire::InstalledExtension;

use super::plugin::{self, Detail, SettingsLoad, field_text};
use crate::design::{Palette, motion};
use crate::icons;
use crate::pages::common::controls::{
    Look, Size, button_sized, icon_button_sized, keyed_setting_row, outlined_tag, refresh_icon,
    spinner_line,
};
use crate::pages::common::ts::{MONO_XS, sm, sm_medium, xs};
use crate::pages::common::{
    SM, Tone, XL, XS, block_fill, empty_state, group_header, label, loading, notice_line,
    page_body, para, rows_card, switch,
};
use crate::plugins::{Density, MountKey};
use crate::state::State;

/// A dirty setting's Save button arriving beside its field: a short slide
/// from the left with a fade, leaving faster.
fn save_slide() -> rune::Transition {
    rune::Transition::offset(-8., 0.)
        .combined(rune::Transition::opacity())
        .springs(rune::SpringPair::with_exit_ratio(motion::SLIDE, 0.6))
}

/// The lifecycle state as a tag on the plugin's card.
pub fn state_tag(p: &Palette, state: PluginState) -> El {
    match state {
        PluginState::Active => outlined_tag(p, Tone::Success, "Active"),
        PluginState::SetupNeeded => outlined_tag(p, Tone::Warning, "Needs setup"),
        PluginState::ConsentRequired => outlined_tag(p, Tone::Warning, "Waiting for you"),
        PluginState::Tampered => outlined_tag(p, Tone::Danger, "Changed"),
        PluginState::Uninstalled => outlined_tag(p, Tone::Secondary, "Removed"),
        PluginState::Other => outlined_tag(p, Tone::Secondary, "Unavailable"),
    }
}

/// Under a held plugin's head: why nothing of it runs, and what to do.
pub fn held_note(s: &State, k: &Kit, ix: usize, e: &InstalledExtension) -> Option<El> {
    let p = &s.palette;
    let id = e.id().to_string();
    let review = |label: &str, look: Look| {
        let id = id.clone();
        button_sized(label, look, Size::Small)
            .on_press(move |s: &mut State| plugin::open_consent(s, id.clone()))
            .build(k)
            .key(format!("review-{ix}"))
            .press_scale(motion::PRESS)
    };
    let (line, ink, buttons) = match e.installation_state {
        PluginState::ConsentRequired => (
            "It runs only after you allow what it asks for.",
            p.text_muted,
            vec![review("Review", Look::Primary)],
        ),
        PluginState::Tampered => {
            let remove_id = id.clone();
            (
                "It changed after you allowed it, so it is stopped until you review it again.",
                p.warning,
                vec![
                    review("Review again", Look::Outline),
                    button_sized("Remove", Look::Danger, Size::Small)
                        .on_press(move |s: &mut State| super::remove(s, remove_id.clone()))
                        .build(k)
                        .key(format!("tampered-remove-{ix}"))
                        .press_scale(motion::PRESS),
                ],
            )
        }
        _ => return None,
    };
    Some(
        row()
            .key(format!("held-{ix}"))
            .w_full()
            .gap(12.)
            .pt(4.)
            .center_y()
            .child(para(line, SM, ink).grow(1.).min_w(0.))
            .child(row().gap(8.).children(buttons)),
    )
}

fn request_icon(kind: RequestKind) -> &'static rune::Icon {
    match kind {
        RequestKind::Tool => &icons::ZAP,
        RequestKind::Network => &icons::GLOBE,
        RequestKind::Credential => &icons::SHIELD_CHECK,
        RequestKind::Hook => &icons::ACTIVITY,
        RequestKind::UiSlot => &icons::LAYOUT_GRID,
        RequestKind::Setting => &icons::SETTINGS,
        RequestKind::Service => &icons::SETTINGS_2,
        RequestKind::Other => &icons::INFO,
    }
}

/// The consent sheet: what the plugin asks for, Allow / Don't allow.
pub fn consent_sheet(s: &State, k: &Kit, layer: &mut Vec<El>) {
    let p = &s.palette;
    let ext = &s.pages.extensions;
    let Some(sheet) = &ext.consent else { return };
    // Drawn from what the sheet captured when it opened, which is also
    // what Allow grants.
    let name = &sheet.name;
    let tampered = sheet.tampered;
    let (title, lead) = if tampered {
        (
            format!("{name} changed"),
            "It changed after you allowed it. This is what it asks for now:".to_string(),
        )
    } else {
        (
            format!("Allow {name}?"),
            format!("Before it runs, {name} asks to:"),
        )
    };
    let requests = sheet
        .shown
        .as_ref()
        .map(|c| c.requests.as_slice())
        .unwrap_or(&[]);
    // The kit's dialog pads its header only: the body lines up with it.
    let mut body = col().w_full().px(16.).pb(16.).gap(12.);
    if requests.is_empty() {
        body = body.child(para("It did not say what it asks for.", SM, p.text_muted));
    } else {
        let rows = requests.iter().enumerate().map(|(ix, r)| {
            row()
                .key(format!("consent-request-{ix}"))
                .w_full()
                .px(14.)
                .py(10.)
                .gap(12.)
                .items_start()
                .child(
                    el().pt(2.).child(
                        icon(request_icon(r.kind), 16.)
                            .color(p.text_muted)
                            .a11y_hidden(),
                    ),
                )
                .child(
                    col()
                        .grow(1.)
                        .min_w(0.)
                        .gap(2.)
                        .child(text(r.title.clone()).font(sm_medium()).color(p.text).wrap())
                        .child(text(r.detail.clone()).font(xs()).color(p.text_muted).wrap()),
                )
        });
        body = body.child(rows_card(p, rows));
    }
    if let Some(digest) = sheet.shown.as_ref().map(|c| c.digest.as_str()) {
        let short: String = digest.chars().take(12).collect();
        body = body.child(
            text(format!("Package {short}"))
                .font(MONO_XS)
                .color(p.text_faint)
                .help("Allowing pins this exact package; a later change asks again."),
        );
    }
    if let Some(err) = &sheet.error {
        body = body.child(
            para(err.clone(), SM, p.danger)
                .key("consent-error")
                .transition(motion::reveal()),
        );
    }
    let mut buttons = row().w_full().gap(8.).center_y();
    if tampered {
        let id = sheet.id.clone();
        buttons = buttons.child(
            button_sized("Remove", Look::Danger, Size::Small)
                .on_press(move |s: &mut State| {
                    plugin::close_consent(s);
                    super::remove(s, id.clone());
                })
                .build(k)
                .key("consent-remove")
                .press_scale(motion::PRESS),
        );
    }
    buttons = buttons
        .child(el().grow(1.))
        .child(
            button_sized("Don't allow", Look::Secondary, Size::Small)
                .on_press(|s: &mut State| plugin::close_consent(s))
                .build(k)
                .key("consent-deny")
                .press_scale(motion::PRESS),
        )
        .child(
            loading(
                button_sized("Allow", Look::Primary, Size::Small)
                    .disabled(sheet.shown.is_none())
                    .on_press(|s: &mut State| plugin::grant(s)),
                sheet.granting,
            )
            .build(k)
            .key("consent-allow")
            .press_scale(motion::PRESS),
        );
    Dialog::new("plugin-consent", title)
        .open(true)
        .fit()
        .width(440.)
        .description(lead)
        .child(body.child(el().h(4.)).child(buttons))
        .on_close(|s: &mut State| plugin::close_consent(s))
        .build(k, layer);
}

/// A plugin's own page: back to the list, its state, settings and pages.
pub fn detail_page(s: &State, k: &Kit, d: &Detail) -> El {
    let p = &s.palette;
    let ext = &s.pages.extensions;
    let plugin = ext.installed.iter().find(|e| e.id() == d.id);
    let name = plugin.map_or(d.id.as_str(), |e| e.display_name.as_str());
    let reopen = d.id.clone();
    let head = row()
        .key("page-header")
        .w_full()
        .gap(8.)
        .center_y()
        .pb(8.)
        .child(
            icon_button_sized(&icons::ARROW_LEFT, "All plugins", Size::Small)
                .on_press(|s: &mut State| plugin::close_detail(s))
                .build(k)
                .key("plugin-back")
                .press_scale(motion::PRESS),
        )
        .child(label(name.to_string(), XL, 500., p.text).grow(1.).min_w(0.))
        .child(
            refresh_icon()
                .on_press(move |s: &mut State| plugin::open_detail(s, reopen.clone()))
                .build(k)
                .key("plugin-refresh")
                .press_scale(motion::PRESS),
        );
    let mut body = col().w_full().child(head);
    if let Some(n) = &ext.notice {
        body = body.child(notice_line(n, p.text_muted));
    }
    if let Some(e) = plugin {
        let mut about = row()
            .key("plugin-about")
            .w_full()
            .gap(8.)
            .center_y()
            .child(state_tag(p, e.installation_state));
        if !e.description.is_empty() {
            about = about.child(
                text(e.description.clone())
                    .font(sm())
                    .color(p.text_muted)
                    .truncate()
                    .grow(1.)
                    .min_w(0.),
            );
        }
        body = body.child(about);
        if let Some(note) = held_note(s, k, 0, e) {
            body = body.child(note);
        }
    }
    let pages: Vec<_> = s
        .plugins
        .of(SlotKind::SettingsPage)
        .filter(|x| x.extension_id == d.id)
        .collect();
    let settings = match &d.settings {
        SettingsLoad::Loading => Some(
            col()
                .key("plugin-settings-loading")
                .transition(motion::fade())
                .pt(32.)
                .child(spinner_line(p, k, "Loading settings")),
        ),
        SettingsLoad::None => None,
        SettingsLoad::Failed(reason) => Some(
            col()
                .key("plugin-settings-failed")
                .transition(motion::fade())
                .child(group_header(p, "Settings", None))
                .child(para(reason.clone(), SM, p.text_muted)),
        ),
        SettingsLoad::Ready(list) => Some(settings_groups(s, k, d, list)),
    };
    let empty = settings.is_none() && pages.is_empty();
    if let Some(x) = settings {
        body = body.child(x);
    }
    for slot in pages {
        let key = MountKey::new(slot, None);
        body = body
            .child(group_header(p, &slot.title, None).key(format!("plugin-page-{}", slot.slot_id)))
            .child(
                col()
                    .key(format!("plugin-page-body-{}", slot.slot_id))
                    .w_full()
                    .padding(16.)
                    .radius(12.)
                    .fill(block_fill(p))
                    .child(crate::plugins::mount_view(s, k, &key, Density::Page)),
            );
    }
    if empty {
        body = body.child(empty_state(p, "Nothing to set for this plugin."));
    }
    page_body("plugin-page", body)
}

/// Settings under their sections, in the schema's order; the unsectioned
/// ones first, under "Settings".
fn settings_groups(s: &State, k: &Kit, d: &Detail, list: &[PluginSetting]) -> El {
    let p = &s.palette;
    let mut groups: Vec<(Option<&str>, Vec<&PluginSetting>)> = Vec::new();
    for x in list.iter().filter(|x| x.kind != SettingKind::Other) {
        let section = x.section.as_deref().filter(|t| !t.trim().is_empty());
        match groups.iter_mut().find(|(g, _)| *g == section) {
            Some((_, items)) => items.push(x),
            None => groups.push((section, vec![x])),
        }
    }
    groups.sort_by_key(|(g, _)| g.is_some());
    let mut out = col()
        .key("plugin-settings")
        .transition(motion::fade())
        .w_full();
    for (section, items) in groups {
        let title = section.unwrap_or("Settings");
        out = out
            .child(group_header(p, title, None))
            .child(rows_card(p, items.into_iter().map(|x| setting(s, k, d, x))));
    }
    out
}

fn setting(s: &State, k: &Kit, d: &Detail, x: &PluginSetting) -> El {
    let p = &s.palette;
    let saving = d.saving.is_some();
    let this_saving = d.saving.as_deref() == Some(x.id.as_str());
    let id = x.id.clone();
    let key = format!("plugin-setting-{}", x.id);
    let mut below: Option<El> = None;
    let control = match x.kind {
        SettingKind::Switch => {
            let on = x.effective().as_bool().unwrap_or(false);
            switch(k, on, saving, &x.label, move |s: &mut State| {
                plugin::save(s, id.clone(), Value::Bool(!on))
            })
        }
        SettingKind::Choice => {
            let picked = x.options.iter().position(|o| &o.value == x.effective());
            let plain = x.options.len() <= 4
                && x.options
                    .iter()
                    .all(|o| o.description.is_none() && o.label.chars().count() <= 16);
            if plain {
                let labels: Vec<&str> = x.options.iter().map(|o| o.label.as_str()).collect();
                let values: Vec<Value> = x.options.iter().map(|o| o.value.clone()).collect();
                let mut seg = rune_kit::SegmentedControl::new(&labels, picked).fit();
                if saving {
                    for i in 0..labels.len() {
                        seg = seg.segment_state(i, rune_kit::State::Disabled);
                    }
                } else {
                    seg = seg.on_pick(move |s: &mut State, ix, _: &mut rune::Tx| {
                        if let Some(v) = values.get(ix) {
                            plugin::save(s, id.clone(), v.clone());
                        }
                    });
                }
                row().child(seg.build(k))
            } else {
                let items = x.options.iter().enumerate().map(|(ix, o)| {
                    let (id, v) = (id.clone(), o.value.clone());
                    let mut item = RadioItem::new(o.label.clone(), picked == Some(ix))
                        .disabled(saving)
                        .on_select(move |s: &mut State| plugin::save(s, id.clone(), v.clone()));
                    if let Some(desc) = &o.description {
                        item = item.description(desc.clone());
                    }
                    item.build(k).key(format!("{key}-option-{ix}"))
                });
                below = Some(col().w_full().px(16.).pb(14.).gap(10.).children(items));
                el()
            }
        }
        SettingKind::Text | SettingKind::Number => match d.input(&x.id) {
            Some((ix, edit)) => {
                let changed = edit.text.trim() != field_text(x.effective());
                let mut field = row().gap(8.).center_y().child(
                    TextInput::new(edit)
                        .width(Some(200.))
                        .label(x.label.clone())
                        .placeholder(field_text(&x.default))
                        .invalid(d.errors.contains_key(&x.id))
                        .on_edit(move |s: &mut State| plugin::input(s, ix))
                        .on_submit({
                            let id = id.clone();
                            move |s: &mut State, _| plugin::save_field(s, id.clone())
                        })
                        .build(k)
                        .key(format!("{key}-input"))
                        .press_scale(motion::PRESS),
                );
                if changed || this_saving {
                    // The dirty field's Save slides in beside it, and out
                    // once the save lands. Typing lands its commit at once
                    // (Rune), so the slide shows on the save's reply.
                    field = field.child(
                        loading(
                            button_sized("Save", Look::Primary, Size::Small)
                                .on_press(move |s: &mut State| plugin::save_field(s, id.clone())),
                            this_saving,
                        )
                        .build(k)
                        .key(format!("{key}-save"))
                        .press_scale(motion::PRESS)
                        .transition(save_slide()),
                    );
                }
                field
            }
            None => el(),
        },
        SettingKind::Other => el(),
    };
    let row_el = keyed_setting_row(
        p,
        &format!("{key}-row"),
        &x.label,
        x.description.as_deref(),
        control,
    );
    let mut out = col().key(key.clone()).w_full().child(row_el);
    if let Some(b) = below {
        out = out.child(b);
    }
    if let Some(reason) = d.errors.get(&x.id) {
        out = out.child(
            el().key(format!("{key}-error"))
                .transition(motion::reveal())
                .w_full()
                .px(16.)
                .pb(12.)
                .child(para(reason.clone(), XS, p.danger)),
        );
    }
    out
}
