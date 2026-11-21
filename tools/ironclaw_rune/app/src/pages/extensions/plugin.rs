//! A plugin's own page under Settings → Plugins, and its consent sheet.
//!
//! - **Consent.** A plugin in `consent_required` runs nothing until the
//!   person allows what it asks for; one in `tampered` changed after it was
//!   allowed and is stopped. The sheet lists the requests and grants at the
//!   digest it showed (`grant_consent`), so a package that changes again
//!   meanwhile is refused by the backend, not silently allowed.
//! - **Settings.** Drawn from the plugin's schema (switch, choice, text,
//!   number) and saved one at a time; a refusal shows the plugin's reason
//!   under its row. Switches and choices flip at once and flip back when
//!   refused.
//! - **Pages.** Each `settings.page` slot is rendered when the page opens.
//!
//! A backend without plugin settings answers 404: the page then shows none.

use std::collections::HashMap;

use rune::TextEdit;
use serde_json::Value;
use zlo_client::plugins::{
    Consent, PluginSetting, PluginState, SettingKind, SlotKind, plain_reason,
};

use super::st;
use crate::jobs::{self, Generation};
use crate::state::State;

/// The open consent sheet.
///
/// It holds what it showed — the name, whether the plugin was tampered and
/// the consent it listed — so Allow grants exactly the digest the person
/// read, whatever a refresh of the list did meanwhile.
pub struct ConsentSheet {
    pub id: String,
    pub name: String,
    pub tampered: bool,
    pub shown: Option<Consent>,
    pub granting: bool,
    pub error: Option<String>,
}

pub enum SettingsLoad {
    Loading,
    /// The backend keeps no settings for plugins (404), or this one has none.
    None,
    Failed(String),
    Ready(Vec<PluginSetting>),
}

/// The plugin page that is open.
pub struct Detail {
    pub id: String,
    pub settings: SettingsLoad,
    /// One field per text or number setting, by setting id.
    pub inputs: Vec<(String, TextEdit)>,
    /// The setting being saved.
    pub saving: Option<String>,
    /// A refused save's reason, by setting id.
    pub errors: HashMap<String, String>,
    asked: Generation,
}

impl Detail {
    pub fn input(&self, setting_id: &str) -> Option<(usize, &TextEdit)> {
        self.inputs
            .iter()
            .enumerate()
            .find(|(_, (id, _))| id == setting_id)
            .map(|(ix, (_, e))| (ix, e))
    }
}

/// A text or number setting's value as the field shows it.
pub fn field_text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Number(n) => n.to_string(),
        _ => String::new(),
    }
}

fn fields(settings: &[PluginSetting]) -> Vec<(String, TextEdit)> {
    settings
        .iter()
        .filter(|x| matches!(x.kind, SettingKind::Text | SettingKind::Number))
        .map(|x| (x.id.clone(), TextEdit::new(field_text(x.effective()))))
        .collect()
}

/// A plugin's lifecycle changed: read the list and its slots again.
pub fn lifecycle_changed(s: &mut State) {
    super::refresh(s);
    crate::plugins::refresh_slots(s);
}

// --- consent ---

pub fn open_consent(s: &mut State, id: String) {
    let ext = st(s);
    let Some(plugin) = ext.installed.iter().find(|e| e.id() == id) else {
        return;
    };
    ext.consent = Some(ConsentSheet {
        id,
        name: plugin.display_name.clone(),
        tampered: plugin.installation_state == PluginState::Tampered,
        shown: plugin.consent.clone(),
        granting: false,
        error: None,
    });
}

pub fn close_consent(s: &mut State) {
    st(s).consent = None;
}

pub fn grant(s: &mut State) {
    let ext = st(s);
    let Some(sheet) = ext.consent.as_mut() else {
        return;
    };
    if sheet.granting {
        return;
    }
    let id = sheet.id.clone();
    // The digest the sheet showed, never the list's current one.
    let Some(digest) = sheet.shown.as_ref().map(|c| c.digest.clone()) else {
        sheet.error = Some("This plugin sent nothing to allow. Refresh and try again.".into());
        return;
    };
    let name = sheet.name.clone();
    sheet.granting = true;
    sheet.error = None;
    let job_id = id.clone();
    jobs::run(
        s,
        "plugin consent",
        move |c| async move { c.grant_consent(&job_id, &digest).await },
        move |s, result, tx| {
            // The reply to the person's own action: let it move.
            tx.animated();

            let ext = st(s);
            let Some(sheet) = ext.consent.as_mut().filter(|x| x.id == id) else {
                return;
            };
            match result {
                Ok(reply) => {
                    ext.consent = None;
                    ext.notice = Some(
                        reply
                            .message
                            .unwrap_or_else(|| format!("{name} is allowed.")),
                    );
                    lifecycle_changed(s);
                }
                Err(e) => {
                    sheet.granting = false;
                    sheet.error = Some(plain_reason(&e));
                }
            }
        },
    );
}

// --- the plugin page ---

pub fn open_detail(s: &mut State, id: String) {
    let ext = st(s);
    let mut detail = Detail {
        id: id.clone(),
        settings: SettingsLoad::Loading,
        inputs: Vec::new(),
        saving: None,
        errors: HashMap::new(),
        asked: Generation::default(),
    };
    let asked = detail.asked.bump();
    ext.detail = Some(detail);
    ext.setup = None;
    ext.pairing = None;
    let job_id = id.clone();
    jobs::run(
        s,
        "plugin settings",
        move |c| async move { c.plugin_settings(&job_id).await },
        move |s, result, tx| {
            // The reply to the person's own action: let it move.
            tx.animated();

            let Some(d) = st(s).detail.as_mut().filter(|d| d.asked == asked) else {
                return;
            };
            d.settings = match result {
                Ok(list) if list.settings.is_empty() => SettingsLoad::None,
                Ok(list) => {
                    d.inputs = fields(&list.settings);
                    SettingsLoad::Ready(list.settings)
                }
                Err(e) if e.is_not_found() => SettingsLoad::None,
                Err(e) => SettingsLoad::Failed(plain_reason(&e)),
            };
        },
    );
    // Its pages render as it opens.
    let pages: Vec<_> = s
        .plugins
        .of(SlotKind::SettingsPage)
        .filter(|x| x.extension_id == id)
        .map(|x| crate::plugins::MountKey::new(x, None))
        .collect();
    for key in pages {
        crate::plugins::render(s, key);
    }
}

pub fn close_detail(s: &mut State) {
    st(s).detail = None;
}

/// The `ix`th field of the open page (or the scratch field once it closed).
pub fn input(s: &mut State, ix: usize) -> &mut TextEdit {
    let ext = st(s);
    match ext.detail.as_mut().and_then(|d| d.inputs.get_mut(ix)) {
        Some((_, edit)) => edit,
        None => &mut ext.scratch,
    }
}

/// Save a switch or choice: it shows the new value at once.
pub fn save(s: &mut State, setting_id: String, value: Value) {
    let Some(d) = st(s).detail.as_mut() else {
        return;
    };
    if d.saving.is_some() {
        return;
    }
    let SettingsLoad::Ready(list) = &mut d.settings else {
        return;
    };
    let Some(setting) = list.iter_mut().find(|x| x.id == setting_id) else {
        return;
    };
    let before = std::mem::replace(&mut setting.value, value.clone());
    put(s, setting_id, value, Some(before), None);
}

/// Save a text or number field from what is typed.
pub fn save_field(s: &mut State, setting_id: String) {
    let Some(d) = st(s).detail.as_mut() else {
        return;
    };
    let SettingsLoad::Ready(list) = &d.settings else {
        return;
    };
    let Some(setting) = list.iter().find(|x| x.id == setting_id) else {
        return;
    };
    let Some((_, edit)) = d.input(&setting_id) else {
        return;
    };
    let typed = edit.text.trim().to_string();
    if typed == field_text(setting.effective()) {
        return;
    }
    let value = if setting.kind == SettingKind::Number {
        match parse_number(&typed) {
            Some(v) => v,
            None => {
                d.errors.insert(setting_id, "Enter a number.".into());
                return;
            }
        }
    } else {
        Value::String(typed.clone())
    };
    if d.saving.is_some() {
        return;
    }
    put(s, setting_id, value, None, Some(typed));
}

/// A typed number as JSON: whole numbers stay integers.
pub fn parse_number(typed: &str) -> Option<Value> {
    if let Ok(n) = typed.parse::<i64>() {
        return Some(Value::from(n));
    }
    let f = typed.parse::<f64>().ok().filter(|f| f.is_finite())?;
    serde_json::Number::from_f64(f).map(Value::Number)
}

/// PUT one setting. `before` is the value to put back when it is refused
/// (switches and choices, which showed the new one at once). `typed` is a
/// field's text as it was sent: the reply rewrites the field only if the
/// person has not typed on since.
fn put(
    s: &mut State,
    setting_id: String,
    value: Value,
    before: Option<Value>,
    typed: Option<String>,
) {
    let Some(d) = st(s).detail.as_mut() else {
        return;
    };
    d.saving = Some(setting_id.clone());
    d.errors.remove(&setting_id);
    let (plugin, asked) = (d.id.clone(), d.asked);
    let job_setting = setting_id.clone();
    jobs::run(
        s,
        "plugin setting",
        move |c| async move { c.put_plugin_setting(&plugin, &job_setting, &value).await },
        move |s, result, tx| {
            // The reply to the person's own action: let it move.
            tx.animated();

            let Some(d) = st(s).detail.as_mut().filter(|d| d.asked == asked) else {
                return;
            };
            d.saving = None;
            let SettingsLoad::Ready(list) = &mut d.settings else {
                return;
            };
            let Some(ix) = list.iter().position(|x| x.id == setting_id) else {
                return;
            };
            match result {
                Ok(updated) if updated.id == setting_id => {
                    let shown = field_text(updated.effective());
                    list[ix] = updated;
                    if let Some((_, edit)) = d.inputs.iter_mut().find(|(id, _)| *id == setting_id)
                        && edit.text != shown
                        && typed.as_deref().is_none_or(|sent| edit.text.trim() == sent)
                    {
                        edit.replace_text(shown);
                    }
                }
                // A reply about another setting: keep what was sent.
                Ok(_) => {}
                Err(e) => {
                    if let Some(before) = before {
                        list[ix].value = before;
                    }
                    d.errors.insert(setting_id, plain_reason(&e));
                }
            }
        },
    );
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::parse_number;

    #[test]
    fn typed_numbers_keep_integers_whole() {
        assert_eq!(parse_number("25"), Some(json!(25)));
        assert_eq!(parse_number("2.5"), Some(json!(2.5)));
        assert_eq!(parse_number("-3"), Some(json!(-3)));
        assert_eq!(parse_number("lots"), None);
        assert_eq!(parse_number("inf"), None);
    }
}
