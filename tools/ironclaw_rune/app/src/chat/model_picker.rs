//! The composer's model pill and its popover (t3code's picker): a rail of
//! providers on the left, search, then models grouped by provider. Any
//! provider the gateway's catalog lists can be picked, mid-thread too: the
//! backend switches in place, moves an unused thread, or continues in a fork
//! carrying the context (`POST /threads/{id}/route`). Port of GPUI
//! `chat/model_picker.rs`; the view is in `model_picker/view.rs`.
//!
//! Without a thread (the new-thread screen) the pick is kept locally and the
//! catalog is read through any existing thread; the catalog is per user, so
//! every thread returns the same list.
//!
//! Like t3code, the user can star models (the star tab lists them) and hide
//! models or whole providers (hover icons on a row or a group title;
//! double- or right-click a rail icon). Hidden ones stay out of the picker
//! until the footer's "Show hidden" brings them back dimmed, to restore.
//! The preferences live in the backend settings, written through the same
//! route. Dragging a rail icon reorders the providers (a local pref).
//!
//! There is no "Default" row: IronClaw lists its profile model first, and a
//! new thread preselects the last pick (or that profile model) by name.
//!
//! Beside the model pill, a thinking pill picks the reasoning effort when the
//! model has one (t3code's "Reasoning" option): the levels come from the
//! catalog per model, and the pick rides the same route, per thread.
//!
//! # Entry points
//!
//! - `start(s)`: after `chat::open`, binds the chat's picker to its thread and
//!   loads the route (area A calls it; the chat stub already does).
//! - `pills(s, cx, k, layer) -> El`: the model and thinking pills for the open
//!   chat's composer, popovers pushed into `layer`.
//! - `open(s)`: ⇧⌘M (area B, `workspace::on_key`) toggles the open chat's picker.
//! - `model_label(&ModelPickerState) -> String`, `pretty_model(id)`,
//!   `provider_mark(..)` for other views.
//! - The draft screen (area B) keeps its own `ModelPickerState::new(None,
//!   catalog_thread)` and renders it with `view::pills_for(s, Slot { .. }, ..)`;
//!   `pending()`, `pending_effort()`, `set_profile(..)` read and steer it.

mod view;

use ironclaw_gpui_client::orchestration::{PickerPrefs, RouteChange, RouteProvider, ThreadRoute};
use rune::TextEdit;

use crate::jobs::{self, Generation};
use crate::state::State;

// `pills_for` and `provider_mark` are for the draft screen (area B) and other views.
#[allow(unused_imports)]
pub use view::{pills, pills_for, provider_mark};

/// IronClaw's own loop; its first model is the profile's.
pub(crate) const NATIVE: &str = "ironclaw";
/// Recent picks shown first in "All", after MonoCode's `pickers.rs`.
const RECENTS: usize = 6;

/// What the picker tells its owner (GPUI `PickerEvent`).
pub enum PickerEvent {
    /// The backend applied a pick. `fork_thread_id` set: open that thread.
    Changed(RouteChange),
    Failed(String),
}

/// How handlers find a picker again from the root state, and where its
/// events go. The chat's is [`CHAT`]; the draft screen passes its own.
#[derive(Clone, Copy)]
pub struct Slot {
    pub get: fn(&State) -> Option<&ModelPickerState>,
    pub get_mut: fn(&mut State) -> Option<&mut ModelPickerState>,
    pub event: fn(&mut State, PickerEvent),
}

/// The open chat's picker.
pub const CHAT: Slot = Slot {
    get: |s| s.chat.as_ref().map(|c| &c.model_picker),
    get_mut: |s| s.chat.as_mut().map(|c| &mut c.model_picker),
    event: chat_event,
};

/// The rail tab: starred models, every provider, or one provider.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) enum Rail {
    Favorites,
    #[default]
    All,
    Provider(String),
}

#[derive(Default)]
pub struct ModelPickerState {
    /// `None` on the new-thread screen.
    pub thread_id: Option<String>,
    /// The thread the catalog and the preferences are read and written through.
    pub catalog_thread: Option<String>,
    pub(crate) route: Option<ThreadRoute>,
    pub(crate) prefs: PickerPrefs,
    /// `(provider, model)` shown on the pill; `model` "default" (a thread
    /// with no pick) reads as IronClaw's profile model.
    pub(crate) current: Option<(String, String)>,
    pub(crate) open: bool,
    /// A narrow composer (a side chat): pills show their icon only, the
    /// name in a tooltip, and the popovers keep inside the window.
    pub compact: bool,
    pub(crate) rail: Rail,
    /// Show hidden providers and models (dimmed) so they can be restored.
    pub(crate) show_hidden: bool,
    /// The reasoning effort; `None`: the provider's default.
    pub(crate) effort: Option<String>,
    pub(crate) effort_open: bool,
    pub(crate) busy: bool,
    /// The draft's profile default `(provider, model)`; it wins over the
    /// recent picks until the user picks by hand.
    profile_pick: Option<(String, String)>,
    /// The profile last asked for; an older profile's answer landing after
    /// it is dropped.
    profile_req: Option<String>,
    /// The user picked on the new-thread screen.
    touched: bool,
    pub(crate) search: TextEdit,
    /// ⌘ is held while the popover is open: the first nine rows show ⌘N.
    pub(crate) cmd_held: bool,
    /// A rail icon being dragged to reorder: its provider and the pointer's
    /// vertical travel.
    pub(crate) rail_drag: Option<(String, f32)>,
    /// Invalidates replies from before a rebind.
    generation: Generation,
}

/// A list group: title, its provider (`None`: no hide button), and the
/// `(provider, model)` rows under it.
pub(crate) type Group<'a> = (String, Option<&'a RouteProvider>, Vec<(&'a RouteProvider, String)>);

impl ModelPickerState {
    /// A picker for `thread_id` (`None`: the new-thread screen), reading the
    /// catalog through `catalog_thread` when there is no thread. Call
    /// [`load`] once it sits in the state.
    pub fn new(thread_id: Option<String>, catalog_thread: Option<String>) -> Self {
        Self {
            catalog_thread: thread_id.clone().or(catalog_thread),
            thread_id,
            ..Default::default()
        }
    }

    /// The local pick on the new-thread screen, to apply once it exists.
    /// `None` while the profile's default stands: the gateway already
    /// creates the thread on it.
    pub fn pending(&self) -> Option<(String, String)> {
        if self.profile_pick.is_some() && !self.touched {
            return None;
        }
        self.current.clone()
    }

    /// The local effort pick on the new-thread screen.
    pub fn pending_effort(&self) -> Option<String> {
        self.effort.clone()
    }

    /// The model list or the thinking menu is showing.
    pub fn is_open(&self) -> bool {
        self.open || self.effort_open
    }

    pub fn close(&mut self) {
        self.open = false;
        self.effort_open = false;
        self.cmd_held = false;
    }

    pub fn set_compact(&mut self, compact: bool) {
        self.compact = compact;
    }

    pub fn toggle(&mut self) {
        self.effort_open = false;
        self.open = !self.open;
        self.cmd_held = false;
        if self.open {
            self.search.replace_text("");
            self.search.focused = true;
        }
    }

    pub(crate) fn providers(&self) -> &[RouteProvider] {
        self.route.as_ref().map_or(&[], |r| r.providers.as_slice())
    }

    /// IronClaw's profile model, which its route lists first.
    fn profile_model(&self) -> Option<&str> {
        self.providers()
            .iter()
            .find(|p| p.provider == NATIVE)
            .and_then(|p| p.models.first())
            .map(String::as_str)
    }

    /// The current model by name: "default" becomes the profile model.
    pub(crate) fn current_model(&self) -> Option<&str> {
        let (provider, model) = self.current.as_ref()?;
        match model.as_str() {
            "default" if provider == NATIVE => self.profile_model().or(Some("default")),
            model => Some(model),
        }
    }

    pub(crate) fn current_provider(&self) -> &str {
        self.current.as_ref().map_or(NATIVE, |(provider, _)| provider)
    }

    /// The efforts `model` on `provider` takes; empty when it has no knob.
    pub(crate) fn efforts_of(&self, provider: &str, model: &str) -> &[String] {
        self.providers()
            .iter()
            .find(|p| p.provider == provider)
            .map_or(&[], |p| p.efforts_for(model))
    }

    /// The providers in the user's order; ones not in it keep the catalog's.
    pub(crate) fn ordered_providers<'a>(&'a self, order: &[String]) -> Vec<&'a RouteProvider> {
        let mut providers: Vec<&RouteProvider> = self.providers().iter().collect();
        providers.sort_by_key(|p| order.iter().position(|id| *id == p.provider).unwrap_or(usize::MAX));
        providers
    }

    /// A new thread's pick: the last pick still offered, else IronClaw's
    /// profile model.
    fn default_pick(&self, recent: &[(String, String)]) -> Option<(String, String)> {
        if let Some((provider, model)) = &self.profile_pick
            && let Some(p) = self.providers().iter().find(|p| &p.provider == provider)
        {
            // An unlisted model runs as the provider's first, as the gateway does.
            let model = if p.models.iter().any(|m| m == model) {
                model.clone()
            } else {
                p.models.first().cloned().unwrap_or_default()
            };
            return Some((provider.clone(), model));
        }
        let offered = |provider: &str, model: &str| {
            !self.prefs.provider_hidden(provider)
                && self
                    .providers()
                    .iter()
                    .any(|p| p.provider == provider && p.models.iter().any(|m| m == model))
        };
        recent
            .iter()
            .find(|(p, m)| offered(p, m))
            .cloned()
            .or_else(|| Some((NATIVE.into(), self.profile_model()?.to_string())))
    }

    /// Hidden providers plus hidden models, for the "Show hidden" toggle.
    pub(crate) fn hidden_count(&self) -> usize {
        self.prefs.hidden_providers.len() + self.prefs.hidden_models.len()
    }

    /// The model this thread runs, as its pill reads.
    pub fn model_label(&self) -> String {
        self.label().0
    }

    pub(crate) fn label(&self) -> (String, Option<String>) {
        let provider = self.current.as_ref().map_or(NATIVE, |(p, _)| p.as_str());
        let name = match self.current_model() {
            // No pick yet: the provider's first model is what runs.
            Some("default") | None => self
                .providers()
                .iter()
                .find(|x| x.provider == provider)
                .and_then(|x| x.models.first().map(|m| pretty_model(m)))
                .unwrap_or_else(|| "Model".to_string()),
            Some(model) => pretty_model(model),
        };
        (name, Some(provider.to_string()))
    }

    /// The rail tabs in the order the rail shows them.
    fn rail_tabs(&self, order: &[String]) -> Vec<Rail> {
        let mut tabs = vec![Rail::Favorites, Rail::All];
        tabs.extend(
            self.ordered_providers(order)
                .into_iter()
                .filter(|p| self.show_hidden || !self.prefs.provider_hidden(&p.provider))
                .map(|p| Rail::Provider(p.provider.clone())),
        );
        tabs
    }

    /// The `(provider, model)` pairs the list shows, in order, grouped under
    /// a header each. A `None` provider is a group with no hide button
    /// (favorites, recents).
    pub(crate) fn groups<'a>(&'a self, order: &[String], recent: &[(String, String)]) -> Vec<Group<'a>> {
        let query = self.search.text.trim().to_lowercase();
        let ordered = self.ordered_providers(order);
        let matches = |provider: &RouteProvider, model: &str| {
            query.is_empty()
                || model.to_lowercase().contains(&query)
                || pretty_model(model).to_lowercase().contains(&query)
                || provider.name.to_lowercase().contains(&query)
        };
        let mut groups: Vec<Group> = Vec::new();
        if self.rail == Rail::Favorites {
            let picks = self
                .prefs
                .favorites
                .iter()
                .filter_map(|f| {
                    let provider = self.providers().iter().find(|p| p.provider == f.provider)?;
                    let shown = self.show_hidden || !self.prefs.provider_hidden(&f.provider);
                    (shown && matches(provider, &f.model)).then(|| (provider, f.model.clone()))
                })
                .collect();
            groups.push(("Favorites".into(), None, picks));
            return groups;
        }
        if self.rail == Rail::All && query.is_empty() {
            let picks: Vec<_> = recent
                .iter()
                .filter_map(|(provider, model)| {
                    let p = self.providers().iter().find(|p| p.provider == *provider)?;
                    (!self.prefs.provider_hidden(provider) && !self.prefs.model_hidden(provider, model))
                        .then(|| (p, model.clone()))
                })
                .collect();
            groups.push(("Recent".into(), None, picks));
        }
        for provider in ordered {
            if let Rail::Provider(only) = &self.rail
                && *only != provider.provider
            {
                continue;
            }
            if self.prefs.provider_hidden(&provider.provider) && !self.show_hidden {
                continue;
            }
            let picks = provider
                .models
                .iter()
                .filter(|m| matches(provider, m))
                .filter(|m| self.show_hidden || !self.prefs.model_hidden(&provider.provider, m))
                .map(|m| (provider, m.clone()))
                .collect();
            groups.push((provider.name.clone(), Some(provider), picks));
        }
        groups
    }

    /// The `ix`th pickable row, as ⌘1..⌘9 counts them.
    pub(crate) fn nth_pick(&self, ix: usize, order: &[String], recent: &[(String, String)]) -> Option<(String, String)> {
        self.groups(order, recent)
            .into_iter()
            .flat_map(|(_, _, picks)| picks)
            .nth(ix)
            .map(|(p, m)| (p.provider.clone(), m))
    }
}

/// Bind the open chat's picker to its thread and read the route. Called
/// once per `chat::open`.
pub fn start(s: &mut State) {
    let Some(chat) = s.chat.as_mut() else { return };
    let id = chat.thread_id.clone();
    let compact = chat.model_picker.compact;
    chat.model_picker = ModelPickerState::new(Some(id), None);
    chat.model_picker.compact = compact;
    load(s, CHAT);
}

/// ⇧⌘M: toggle the open chat's picker. Area B wires the shortcut.
pub fn open(s: &mut State) {
    if let Some(p) = (CHAT.get_mut)(s)
        && (p.route.as_ref().is_some_and(|r| !r.providers.is_empty()) || p.current.is_some())
    {
        p.toggle();
    }
}

/// Read the route (catalog, preferences, the thread's pick) through the
/// picker's catalog thread.
pub fn load(s: &mut State, slot: Slot) {
    let Some(p) = (slot.get_mut)(s) else { return };
    let Some(thread_id) = p.catalog_thread.clone() else { return };
    let own = p.thread_id.is_some();
    let generation = p.generation.bump();
    jobs::run(
        s,
        "model picker",
        move |c| async move { c.thread_route(&thread_id).await },
        move |s, result, _| {
            let recent = s.prefs.recent_models.clone();
            let Some(p) = (slot.get_mut)(s).filter(|p| p.generation == generation) else { return };
            // silent-ok: an older gateway has no route; the pill stays hidden.
            let Ok(route) = result else { return };
            if own {
                p.current = route_pick(&route);
                p.effort = route.effort.clone();
            }
            p.prefs = route.picker.clone();
            if !p.prefs.favorites.is_empty() {
                p.rail = Rail::Favorites;
            }
            p.route = Some(route);
            // A draft re-picks once the catalog is in: the profile may have loaded first.
            if p.current.is_none() || (!own && !p.touched) {
                p.current = p.default_pick(&recent);
            }
        },
    );
}

/// The new-thread screen's profile: show its default model unless the user
/// already picked one.
pub fn set_profile(s: &mut State, slot: Slot, profile_id: String) {
    let Some(p) = (slot.get_mut)(s) else { return };
    p.profile_req = Some(profile_id.clone());
    let asked = profile_id.clone();
    jobs::run(
        s,
        "model picker",
        move |c| async move { c.profile(&profile_id).await },
        move |s, result, _| {
            let recent = s.prefs.recent_models.clone();
            let Some(p) = (slot.get_mut)(s) else { return };
            // Opening the draft asks for the last-used profile, then the
            // picked one: only the latest request may set the pill.
            if p.profile_req.as_deref() != Some(asked.as_str()) {
                return;
            }
            // silent-ok: an unreadable profile leaves the recent pick on the pill.
            let Ok(info) = result else { return };
            let st = info.settings;
            p.profile_pick = st.default_provider.map(|pv| (pv, st.default_model.unwrap_or_default()));
            if !p.touched {
                p.current = p.default_pick(&recent);
            }
        },
    );
}

pub(crate) fn pick_effort(s: &mut State, slot: Slot, effort: Option<String>) {
    let Some(p) = (slot.get_mut)(s) else { return };
    p.effort_open = false;
    let previous = std::mem::replace(&mut p.effort, effort.clone());
    let Some(thread_id) = p.thread_id.clone() else { return };
    let generation = p.generation;
    let wire = effort.unwrap_or_else(|| "default".into());
    jobs::run(
        s,
        "model picker",
        move |c| async move { c.set_thread_effort(&thread_id, &wire).await },
        move |s, result, _| {
            if let Err(e) = result {
                if let Some(p) = (slot.get_mut)(s).filter(|p| p.generation == generation) {
                    p.effort = previous;
                }
                (slot.event)(s, PickerEvent::Failed(e.to_string()));
            }
        },
    );
}

pub(crate) fn pick(s: &mut State, slot: Slot, provider: String, model: String) {
    let recent = (provider.clone(), model.clone());
    s.update_prefs(|p| {
        p.recent_models.retain(|r| *r != recent);
        p.recent_models.insert(0, recent);
        p.recent_models.truncate(RECENTS);
    });
    let Some(p) = (slot.get_mut)(s) else { return };
    p.open = false;
    p.cmd_held = false;
    // The effort carries over when the new model takes the same level.
    let effort = p.effort.clone().filter(|e| p.efforts_of(&provider, &model).contains(e));
    let Some(thread_id) = p.thread_id.clone() else {
        p.touched = true;
        p.current = Some((provider, model));
        p.effort = effort;
        return;
    };
    if p.busy {
        return;
    }
    p.busy = true;
    let generation = p.generation;
    let (pv, md) = (provider.clone(), model.clone());
    jobs::run(
        s,
        "model picker",
        move |c| async move { c.set_thread_route(&thread_id, &pv, &md, effort.as_deref()).await },
        move |s, result, _| {
            let Some(p) = (slot.get_mut)(s).filter(|p| p.generation == generation) else { return };
            p.busy = false;
            match result {
                Ok(change) => {
                    if change.fork_thread_id.is_none() {
                        p.current = Some((provider, model));
                        p.effort = change.effort.clone();
                        if let Some(route) = p.route.as_mut() {
                            route.fresh = route.fresh && change.moved != "in_place";
                        }
                    }
                    (slot.event)(s, PickerEvent::Changed(change));
                }
                Err(e) => (slot.event)(s, PickerEvent::Failed(e.to_string())),
            }
        },
    );
}

/// Apply a preference change locally, then save the whole document.
pub(crate) fn edit_prefs(s: &mut State, slot: Slot, edit: impl FnOnce(&mut PickerPrefs)) {
    let Some(p) = (slot.get_mut)(s) else { return };
    edit(&mut p.prefs);
    if p.prefs.favorites.is_empty() && p.rail == Rail::Favorites {
        p.rail = Rail::All;
    }
    let Some(thread_id) = p.catalog_thread.clone() else { return };
    let prefs = p.prefs.clone();
    jobs::run(
        s,
        "model picker",
        move |c| async move { c.set_model_picker(&thread_id, &prefs).await },
        move |s, result, _| {
            if let Err(e) = result {
                (slot.event)(s, PickerEvent::Failed(e.to_string()));
            }
        },
    );
}

pub(crate) fn toggle_provider(s: &mut State, slot: Slot, key: String) {
    if let Some(p) = (slot.get_mut)(s)
        && p.rail == Rail::Provider(key.clone())
    {
        p.rail = Rail::All;
    }
    edit_prefs(s, slot, |prefs| prefs.toggle_provider(&key));
}

/// Move `dragged` into `target`'s place in the rail and the list.
pub(crate) fn reorder(s: &mut State, slot: Slot, dragged: &str, target: &str) {
    let order_pref = s.prefs.provider_order.clone();
    let Some(p) = (slot.get)(s) else { return };
    let mut order: Vec<String> = p.ordered_providers(&order_pref).iter().map(|p| p.provider.clone()).collect();
    let (Some(from), Some(to)) = (
        order.iter().position(|p| p == dragged),
        order.iter().position(|p| p == target),
    ) else {
        return;
    };
    let moved = order.remove(from);
    order.insert(to, moved);
    s.update_prefs(|p| p.provider_order = order);
}

/// Step to the next (or previous) rail tab, in the order the rail shows.
pub(crate) fn cycle_rail(s: &mut State, slot: Slot, forward: bool) {
    let order = s.prefs.provider_order.clone();
    let Some(p) = (slot.get_mut)(s) else { return };
    let mut tabs = p.rail_tabs(&order);
    let n = tabs.len();
    let at = tabs.iter().position(|r| *r == p.rail).unwrap_or(0);
    let next = if forward { (at + 1) % n } else { (at + n - 1) % n };
    p.rail = tabs.swap_remove(next);
}

/// The chat's reaction to its picker (GPUI `ChatView`'s `PickerEvent` subscription).
fn chat_event(s: &mut State, event: PickerEvent) {
    match event {
        PickerEvent::Changed(change) => {
            if let Some(fork) = change.fork_thread_id {
                crate::workspace::open_thread(s, fork);
            } else if change.moved == "switched" {
                // Same thread, new runtime: its live events come from
                // elsewhere now, and the timeline gains the marker.
                crate::chat::resubscribe(s);
                crate::chat::reload_timeline(s);
            } else if change.moved == "in_place" {
                crate::chat::reload_timeline(s);
            }
        }
        PickerEvent::Failed(message) => crate::chat::set_error(s, message),
    }
}

/// The thread's pick as `(provider, model)`; no model is the default.
fn route_pick(route: &ThreadRoute) -> Option<(String, String)> {
    let provider = route.provider.clone().unwrap_or_else(|| NATIVE.into());
    Some((provider, route.model.clone().unwrap_or_else(|| "default".into())))
}

/// The footer's restore toggle: "Show hidden (2 providers, 25 models)".
pub(crate) fn hidden_summary(providers: usize, models: usize) -> String {
    let count = |n: usize, what: &str| match n {
        0 => None,
        1 => Some(format!("1 {what}")),
        n => Some(format!("{n} {what}s")),
    };
    let parts: Vec<String> = [count(providers, "provider"), count(models, "model")]
        .into_iter()
        .flatten()
        .collect();
    format!("Show hidden ({})", parts.join(", "))
}

/// "xhigh" -> "Extra High", as t3code names the levels.
pub(crate) fn effort_label(effort: &str) -> String {
    match effort {
        "xhigh" => "Extra High".into(),
        other => pretty_model(other),
    }
}

/// "claude-opus-5-5" -> "Claude Opus 5.5", "gpt-6.1-sol" -> "GPT 6.1 Sol".
pub fn pretty_model(id: &str) -> String {
    let tail = id.rsplit('/').next().unwrap_or(id);
    let mut out: Vec<String> = Vec::new();
    for part in tail.split(['-', '_']) {
        if part.is_empty() {
            continue;
        }
        let numeric = part.chars().all(|c| c.is_ascii_digit() || c == '.');
        match out.last_mut() {
            Some(last) if numeric && last.chars().all(|c| c.is_ascii_digit() || c == '.') => {
                last.push('.');
                last.push_str(part);
            }
            _ if part.eq_ignore_ascii_case("gpt") => out.push("GPT".into()),
            _ => {
                let mut chars = part.chars();
                let first = chars.next().map(|c| c.to_ascii_uppercase());
                out.push(first.into_iter().chain(chars).collect());
            }
        }
    }
    if out.is_empty() { id.to_string() } else { out.join(" ") }
}

/// A picker with a small catalog, for the headless scenes.
pub fn fixture() -> ModelPickerState {
    use ironclaw_gpui_client::orchestration::PickerModel;
    let provider = |id: &str, name: &str, runtime: &str, models: &[&str], efforts: &[&str]| RouteProvider {
        provider: id.into(),
        name: name.into(),
        runtime: runtime.into(),
        authenticated: id != "openrouter",
        models: models.iter().map(|m| m.to_string()).collect(),
        efforts: efforts.iter().map(|m| m.to_string()).collect(),
        model_efforts: Default::default(),
        brand: None,
    };
    let route = ThreadRoute {
        thread_id: "t1".into(),
        runtime: "claude-cli".into(),
        provider: Some("anthropic".into()),
        model: Some("claude-opus-5-5".into()),
        effort: Some("high".into()),
        fresh: false,
        providers: vec![
            provider(NATIVE, "IronClaw", NATIVE, &["claude-sonnet-5-5"], &[]),
            provider(
                "anthropic",
                "Claude",
                "claude-cli",
                &["claude-opus-5-5", "claude-sonnet-5-5", "claude-haiku-5-5"],
                &["low", "medium", "high", "xhigh"],
            ),
            provider("codex", "Codex", "codex", &["gpt-6.1-sol", "gpt-6.1-mini"], &["minimal", "high"]),
            provider("openrouter", "OpenRouter", NATIVE, &["deepseek/deepseek-v4"], &[]),
        ],
        picker: PickerPrefs {
            hidden_providers: Vec::new(),
            hidden_models: vec![PickerModel::new("codex", "gpt-6.1-mini")],
            favorites: Vec::new(),
        },
    };
    let mut p = ModelPickerState::new(Some("t1".into()), None);
    p.current = route_pick(&route);
    p.effort = route.effort.clone();
    p.prefs = route.picker.clone();
    p.route = Some(route);
    p
}

/// Scene `model-picker` (`effort`: `model-picker-effort`): the open chat
/// with its picker's popover (or the thinking menu) open.
pub fn scene(s: &mut State, effort: bool) {
    crate::scenes::chat(s);
    s.prefs.recent_models = vec![("anthropic".into(), "claude-opus-5-5".into()), ("codex".into(), "gpt-6.1-sol".into())];
    if let Some(c) = s.chat.as_mut() {
        c.model_picker = fixture();
        if effort {
            c.model_picker.effort_open = true;
        } else {
            c.model_picker.toggle();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{effort_label, hidden_summary, pretty_model};

    #[test]
    fn the_hidden_footer_counts_both_kinds() {
        assert_eq!(hidden_summary(3, 25), "Show hidden (3 providers, 25 models)");
        assert_eq!(hidden_summary(1, 0), "Show hidden (1 provider)");
    }

    #[test]
    fn effort_levels_read_like_t3code() {
        assert_eq!(effort_label("xhigh"), "Extra High");
        assert_eq!(effort_label("minimal"), "Minimal");
        assert_eq!(effort_label("max"), "Max");
    }

    #[test]
    fn model_ids_read_like_names() {
        assert_eq!(pretty_model("claude-opus-5-5"), "Claude Opus 5.5");
        assert_eq!(pretty_model("gpt-6.1-sol"), "GPT 6.1 Sol");
        assert_eq!(pretty_model("anthropic/claude-haiku-5-5"), "Claude Haiku 5.5");
    }
}

// The new-thread screen's picker (area B's `workspace/draft.rs`).

/// The draft screen's picker, in `s.workspace.draft`. A failed pick shows
/// as the draft's error; a change needs nothing (the thread does not exist
/// yet, the pick applies when it is created).
pub const DRAFT: Slot = Slot {
    get: |s| s.workspace.draft.as_ref().map(|d| &d.picker),
    get_mut: |s| s.workspace.draft.as_mut().map(|d| &mut d.picker),
    event: |s, event| {
        if let (PickerEvent::Failed(message), Some(d)) = (event, s.workspace.draft.as_mut()) {
            d.error = Some(message);
        }
    },
};

/// The draft screen picked a profile: a fresh picker reading the catalog
/// through `catalog_thread`, on that profile's default.
pub fn draft_set_profile(s: &mut State, profile: String, catalog_thread: Option<String>) {
    let Some(d) = s.workspace.draft.as_mut() else { return };
    if d.picker.catalog_thread.is_none() {
        d.picker = ModelPickerState::new(None, catalog_thread);
    }
    set_profile(s, DRAFT, profile);
}

/// The draft screen's model and thinking pills.
pub fn draft_pill(
    s: &State,
    cx: &mut rune::Cx<State>,
    k: &rune_kit::Kit,
    layer: &mut Vec<rune::El>,
) -> rune::El {
    pills_for(s, DRAFT, cx, k, layer)
}

pub fn pending(picker: &ModelPickerState) -> Option<(String, String)> {
    picker.pending()
}

pub fn pending_effort(picker: &ModelPickerState) -> Option<String> {
    picker.pending_effort()
}

/// Esc: close whichever picker is open (chat or draft). True when one was.
pub fn close_open(s: &mut State) -> bool {
    for slot in [CHAT, DRAFT] {
        if let Some(p) = (slot.get_mut)(s)
            && p.is_open()
        {
            p.close();
            return true;
        }
    }
    false
}
