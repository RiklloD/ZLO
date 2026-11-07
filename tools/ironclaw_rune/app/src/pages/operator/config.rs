//! Operator → Config: keys with source and precedence, and edit one key as
//! JSON (read fresh first, confirm before saving). GPUI
//! `pages/operator/config.rs`.
//!
//! No "Validate" button: the gateway's validate route has no effective-config
//! service behind it and answers "not wired yet" for every key.

use crate::pages::common::{Tone, empty_state, error_line, loading};
use crate::pages::common::controls::{Look, Size, button_sized, input, muted, outlined_tag, rule, spinner};
use crate::pages::common::ts::{MONO_XS, SM_MEDIUM};
use ironclaw_gpui_client::product_wire::{
    RebornOperatorConfigEntry, RebornOperatorConfigListResponse,
};
use rune::{El, TextEdit, col, row, text};
use rune_kit::Kit;

use super::{confirm_row, diagnostic_row, notice_line, section_title};
use crate::jobs;
use crate::state::State;

const VALUE_PREVIEW_CHARS: usize = 120;

pub struct Editor {
    pub key: String,
    pub input: TextEdit,
    pub confirming: bool,
    pub saving: bool,
}

pub struct ConfigPanel {
    pub list: Option<RebornOperatorConfigListResponse>,
    pub filter: TextEdit,
    pub editor: Option<Editor>,
    pub error: Option<String>,
    pub notice: Option<String>,
    /// The editor field's accessor target once the editor closed.
    pub(super) scratch: TextEdit,
}

impl Default for ConfigPanel {
    fn default() -> Self {
        Self {
            list: None,
            filter: TextEdit::new(""),
            editor: None,
            error: None,
            notice: None,
            scratch: TextEdit::new(""),
        }
    }
}

fn st(s: &mut State) -> Option<&mut ConfigPanel> {
    s.pages.operator.config.as_mut()
}

fn filter_edit(s: &mut State) -> &mut TextEdit {
    let o = &mut s.pages.operator;
    match o.config.as_mut() {
        Some(p) => &mut p.filter,
        None => &mut o.status_scratch,
    }
}

fn editor_edit(s: &mut State) -> &mut TextEdit {
    let o = &mut s.pages.operator;
    match o.config.as_mut() {
        Some(p) => match p.editor.as_mut() {
            Some(e) => &mut e.input,
            None => &mut p.scratch,
        },
        None => &mut o.status_scratch,
    }
}

pub fn refresh(s: &mut State) {
    jobs::run(
        s,
        "operator",
        |c| async move { c.operator_config().await },
        |s, result, _| {
            let Some(p) = st(s) else { return };
            match result {
                Ok(list) => {
                    p.list = Some(list);
                    p.error = None;
                }
                Err(e) => p.error = Some(e.to_string()),
            }
        },
    );
}

/// Read the key fresh (`get_config_key`) so the editor starts from the
/// stored value, not a stale list row.
fn edit(s: &mut State, key: String) {
    if let Some(p) = st(s) {
        p.notice = None;
    }
    let job_key = key.clone();
    jobs::run(
        s,
        "operator",
        move |c| async move { c.operator_config_key(&job_key).await },
        move |s, result, _| {
            let Some(p) = st(s) else { return };
            match result {
                Ok(resp) => {
                    let text = if resp.entry.redacted {
                        String::new()
                    } else {
                        resp.entry.value.to_string()
                    };
                    let mut input = TextEdit::new(text);
                    input.focused = true;
                    replace_row(p, resp.entry);
                    p.editor = Some(Editor {
                        key,
                        input,
                        confirming: false,
                        saving: false,
                    });
                }
                Err(e) => p.error = Some(e.to_string()),
            }
        },
    );
}

fn ask_save(s: &mut State) {
    let Some(p) = st(s) else { return };
    let Some(editor) = p.editor.as_mut() else {
        return;
    };
    match serde_json::from_str::<serde_json::Value>(&editor.input.text) {
        Ok(_) => {
            editor.confirming = true;
            p.error = None;
        }
        Err(e) => p.error = Some(format!("Not valid JSON: {e}")),
    }
}

fn save(s: &mut State) {
    let Some(p) = st(s) else { return };
    let Some(editor) = p.editor.as_mut() else {
        return;
    };
    let value = match serde_json::from_str::<serde_json::Value>(&editor.input.text) {
        Ok(value) => value,
        Err(e) => {
            p.error = Some(format!("Not valid JSON: {e}"));
            return;
        }
    };
    editor.confirming = false;
    editor.saving = true;
    let key = editor.key.clone();
    jobs::run(
        s,
        "operator",
        move |c| async move { c.set_operator_config_key(&key, value).await },
        |s, result, _| {
            let Some(p) = st(s) else { return };
            match result {
                // The response is the gateway's read-back of the stored key.
                Ok(resp) => {
                    p.notice = Some(format!("Saved {}", resp.entry.key));
                    replace_row(p, resp.entry);
                    p.editor = None;
                    p.error = None;
                }
                Err(e) => {
                    if let Some(editor) = p.editor.as_mut() {
                        editor.saving = false;
                    }
                    p.error = Some(e.to_string());
                }
            }
        },
    );
}

fn replace_row(p: &mut ConfigPanel, entry: RebornOperatorConfigEntry) {
    if let Some(row) = p
        .list
        .as_mut()
        .and_then(|l| l.entries.iter_mut().find(|e| e.key == entry.key))
    {
        *row = entry;
    }
}

fn render_row(
    s: &State,
    k: &Kit,
    panel: &ConfigPanel,
    ix: usize,
    entry: &RebornOperatorConfigEntry,
) -> El {
    let p = &s.palette;
    let editing = panel.editor.as_ref().filter(|e| e.key == entry.key);
    let value = if entry.redacted {
        "(secret, hidden)".to_string()
    } else {
        preview(&entry.value)
    };
    let mut head = row()
        .w_full()
        .gap(8.)
        .center_y()
        .child(
            col()
                .grow(1.)
                .min_w(0.)
                .child(
                    text(entry.key.clone())
                        .font(SM_MEDIUM)
                        .color(p.text)
                        .truncate(),
                )
                .child(text(value).font(MONO_XS).color(p.text_muted).truncate()),
        )
        .child(outlined_tag(p, Tone::Secondary, entry.source.clone()));
    if !entry.mutable {
        head = head.child(outlined_tag(p, Tone::Secondary, "Read-only"));
    }
    if entry.mutable && editing.is_none() {
        let key = entry.key.clone();
        head = head.child(
            button_sized("Edit", Look::Ghost, Size::XSmall)
                .on_press(move |s: &mut State| edit(s, key.clone()))
                .build(k)
                .key(format!("edit-key-{ix}")),
        );
    }
    let mut c = col()
        .key(format!("config-{}", entry.key))
        .w_full()
        .gap(4.)
        .py(8.)
        .child(head);
    if let Some(editor) = editing {
        c = c.child(
            row()
                .w_full()
                .gap(8.)
                .center_y()
                .child(
                    col().grow(1.).min_w(0.).child(
                        input(&editor.input)
                            .placeholder("JSON value, e.g. true or {\"state\": \"ask_each_time\"}")
                            .on_edit(editor_edit)
                            .build(k)
                            .key(format!("key-editor-{ix}")),
                    ),
                )
                .child(
                    loading(
                        button_sized("Save", Look::Primary, Size::Small)
                            .disabled(editor.confirming)
                            .on_press(|s: &mut State| ask_save(s)),
                        editor.saving,
                    )
                    .build(k)
                    .key(format!("save-key-{ix}")),
                )
                .child(
                    button_sized("Cancel", Look::Ghost, Size::Small)
                        .on_press(|s: &mut State| {
                            if let Some(p) = st(s) {
                                p.editor = None;
                            }
                        })
                        .build(k)
                        .key(format!("cancel-edit-{ix}")),
                ),
        );
        if editor.confirming {
            c = c.child(confirm_row(
                s,
                k,
                format!(
                    "Change {}? This is an operator setting and applies to every chat and automation.",
                    editor.key
                ),
                button_sized("Change", Look::Danger, Size::Small).on_press(|s: &mut State| save(s)),
                button_sized("Cancel", Look::Ghost, Size::Small).on_press(|s: &mut State| {
                    if let Some(e) = st(s).and_then(|p| p.editor.as_mut()) {
                        e.confirming = false;
                    }
                }),
            ));
        }
    }
    col().w_full().child(c).child(rule(p))
}

/// One-line JSON preview, cut on a char boundary.
fn preview(value: &serde_json::Value) -> String {
    let text = value.to_string();
    if text.chars().count() <= VALUE_PREVIEW_CHARS {
        return text;
    }
    let cut: String = text.chars().take(VALUE_PREVIEW_CHARS).collect();
    format!("{cut}…")
}

pub fn view(s: &State, k: &Kit, panel: &ConfigPanel) -> El {
    let p = &s.palette;
    let needle = panel.filter.text.trim().to_lowercase();
    let (rows, precedence, diagnostics): (Vec<El>, String, Vec<El>) = match panel.list.as_ref() {
        Some(list) => (
            list.entries
                .iter()
                .enumerate()
                .filter(|(_, e)| needle.is_empty() || e.key.to_lowercase().contains(&needle))
                .map(|(ix, e)| render_row(s, k, panel, ix, e))
                .collect(),
            list.precedence.join(" > "),
            list.diagnostics
                .iter()
                .map(|d| diagnostic_row(s, d))
                .collect(),
        ),
        None => (Vec::new(), String::new(), Vec::new()),
    };
    let loaded = panel.list.is_some();
    let mut c = col().w_full();
    if let Some(e) = &panel.error {
        c = c.child(error_line(p, e));
    }
    if let Some(n) = &panel.notice {
        c = c.child(notice_line(s, n));
    }
    c = c.child(section_title(s, "Keys"));
    if !precedence.is_empty() {
        c = c.child(muted(p, format!("Precedence: {precedence}")).pb(8.));
    }
    c = c.children(diagnostics).child(
        input(&panel.filter)
            .placeholder("Filter keys")
            .leading_icon(&crate::icons::SEARCH)
            .on_edit(filter_edit)
            .build(k)
            .key("config-filter"),
    );
    if !loaded {
        c = c.child(spinner(k));
    }
    if loaded && rows.is_empty() {
        c = c.child(empty_state(p, "No matching keys."));
    }
    c.child(col().w_full().children(rows))
}

#[cfg(test)]
mod tests {
    use super::preview;

    #[test]
    fn preview_cuts_long_values_on_char_boundaries() {
        assert_eq!(preview(&serde_json::json!(true)), "true");
        let long = serde_json::Value::String("é".repeat(200));
        let out = preview(&long);
        assert!(out.ends_with('…'));
        assert_eq!(out.chars().count(), 121);
    }
}
