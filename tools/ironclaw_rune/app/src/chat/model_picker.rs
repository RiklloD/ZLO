//! The composer's model pill and its popover (t3code's picker): a rail of
//! providers on the left, search, then models grouped by provider. Any
//! provider the gateway's catalog lists can be picked, mid-thread too: the
//! backend switches in place, moves an unused thread, or continues in a fork
//! carrying the context (`POST /threads/{id}/route`). The view is in
//! `model_picker/view.rs`.
//!
//! Without a thread (the new-thread screen) the pick is kept locally and the
//! catalog is read through any existing thread; the catalog is per user, so
//! every thread returns the same list.
//!
//! Like t3code, the user can star models (the star tab lists them).
//! Which models the picker offers is set in Settings → Providers, not
//! here: each connected provider lists its models with a switch, and a
//! removed provider (a CLI agent taken out of ZLO) is gone from the picker
//! altogether. The picker only reads those preferences, re-reading them each
//! time it opens so a change made in Settings shows at once. They live in
//! the backend settings, written through the same route, always re-read
//! before a write (`Client::edit_model_picker`). Dragging a rail icon
//! reorders the providers (a local pref).
//!
//! There is no "Default" row: IronClaw lists its profile model first, and a
//! new thread preselects the last pick (or that profile model) by name.
//!
//! The one pill reads "model · thinking" and its popover holds both: the
//! models above, and the thinking slider below them when the current model
//! has reasoning levels (t3code's "Reasoning" option): drag or click along
//! it, or Tab / Shift+Tab from the search field. The levels come from the
//! catalog per model, and the pick rides the same route, per thread.
//!
//! # Entry points
//!
//! Every chat has its own picker, the open one and each side chat:
//! [`Slot::Chat`] finds it by thread
//! id through `chat::chat_for`, so a reply for a closed chat is dropped.
//!
//! - `start(s, id)`: after a chat starts, binds its picker to its thread and
//!   loads the route.
//! - `pills(s, id, cx, k, layer) -> El`: the model pill for
//!   `id`'s composer, popovers pushed into `layer`.
//! - `open(s)`: ⇧⌘M (`workspace::on_key`) toggles the focused chat's
//!   picker (`focused_slot`: the side chat's while its composer has focus
//!   or its picker is open); `close_open(s)` is Esc.
//! - `pretty_model(id)`,
//!   `provider_mark(..)` for other views.
//! - The draft screen keeps its own `ModelPickerState::new(None,
//!   catalog_thread)` and renders it with `view::pills_for(s, Slot { .. }, ..)`;
//!   `pending()`, `pending_effort()`, `set_profile(..)` read and steer it.

mod matrix;
mod slot;
mod view;

use rune::TextEdit;
use zlo_client::orchestration::{PickerPrefs, RouteChange, RouteProvider, ThreadRoute};

use crate::jobs::{self, Generation};
use crate::state::State;

// `pills_for` and `provider_mark` are for the draft screen and other views.
pub use slot::{DRAFT, FixedSlot, Slot, close_open, focused_slot};
pub use view::{pills, pills_for, provider_mark};

/// IronClaw's own loop; its first model is the profile's.
pub(crate) const NATIVE: &str = "ironclaw";
/// Recent picks shown first in "All", after MonoCode's `pickers.rs`.
const RECENTS: usize = 6;

/// What the picker tells its owner.
pub enum PickerEvent {
    /// The backend applied a pick. `fork_thread_id` set: open that thread.
    Changed(RouteChange),
    Failed(String),
}

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
    /// The thinking slider's handle while it is dragged, as a fraction of
    /// the track; the level is picked when the drag ends.
    pub(crate) thinking_drag: Option<f32>,
    /// The level a thinking drag let go on, picked by [`poll`] in the next
    /// frame (`Some(None)`: Auto).
    pub(crate) thinking_release: Option<Option<String>>,
    /// Bumped by every preference read and edit: only the latest one's
    /// answer may replace `prefs`, so a slow read never brings back a model
    /// hidden since.
    prefs_seq: u64,
    /// The reasoning effort; `None`: the provider's default.
    pub(crate) effort: Option<String>,
    /// Bumped by every effort or model pick: a failed effort write only
    /// rolls back while no later pick has replaced it.
    pub(crate) effort_seq: u64,
    pub(crate) busy: bool,
    /// The draft's profile default `(provider, model)`; it wins over the
    /// recent picks until the user picks by hand.
    profile_pick: Option<(String, String)>,
    /// The profile last asked for; an older profile's answer landing after
    /// it is dropped.
    profile_req: Option<String>,
    /// The user picked on the new-thread screen.
    touched: bool,
    /// A model + effort pick carried across a computer switch on the draft
    /// screen: validated against the new computer's catalog when `route_loaded`
    /// lands, and dropped (falling back to that computer's default) if absent.
    carried_pick: Option<((String, String), Option<String>)>,
    pub(crate) search: TextEdit,
    /// ⌘ is held while the popover is open: the first nine rows show ⌘N.
    pub(crate) cmd_held: bool,
    /// A rail icon being dragged to reorder: its provider and the pointer's
    /// vertical travel.
    pub(crate) rail_drag: Option<(String, f32)>,
    /// The `(provider, model)` snapped to while dragging on the Signal Matrix.
    pub(crate) matrix_drag: Option<(String, String)>,
    /// Invalidates replies from before a rebind.
    generation: Generation,
}

/// A list group: title, its provider (`None`: no hide button), and the
/// `(provider, model)` rows under it.
pub(crate) type Group<'a> = (
    String,
    Option<&'a RouteProvider>,
    Vec<(&'a RouteProvider, String)>,
);

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

    /// A route read was asked for since the picker was made.
    #[cfg(test)]
    pub(crate) fn route_requested(&self) -> bool {
        self.generation != Generation::default()
    }

    /// The popover is showing.
    pub fn is_open(&self) -> bool {
        self.open
    }

    pub fn close(&mut self) {
        self.open = false;
        self.cmd_held = false;
        self.matrix_drag = None;
    }

    pub fn set_compact(&mut self, compact: bool) {
        self.compact = compact;
    }

    pub fn toggle(&mut self) {
        self.open = !self.open;
        self.cmd_held = false;
        self.matrix_drag = None;
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
        self.current
            .as_ref()
            .map_or(NATIVE, |(provider, _)| provider)
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
        providers.sort_by_key(|p| {
            order
                .iter()
                .position(|id| *id == p.provider)
                .unwrap_or(usize::MAX)
        });
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

    /// Whether the composer shows the pill: the catalog is in, or a model
    /// is already picked.
    pub(crate) fn shows_pill(&self) -> bool {
        self.route.as_ref().is_some_and(|r| !r.providers.is_empty()) || self.current.is_some()
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
                .filter(|p| !self.prefs.provider_hidden(&p.provider))
                .map(|p| Rail::Provider(p.provider.clone())),
        );
        tabs
    }

    /// The `(provider, model)` pairs the list shows, in order, grouped under
    /// a header each. A `None` provider is a group with no hide button
    /// (favorites, recents).
    pub(crate) fn groups<'a>(
        &'a self,
        order: &[String],
        recent: &[(String, String)],
    ) -> Vec<Group<'a>> {
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
                    let shown = !self.prefs.provider_hidden(&f.provider)
                        && !self.prefs.model_hidden(&f.provider, &f.model);
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
                    (!self.prefs.provider_hidden(provider)
                        && !self.prefs.model_hidden(provider, model))
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
            if self.prefs.provider_hidden(&provider.provider) {
                continue;
            }
            let picks = provider
                .models
                .iter()
                .filter(|m| matches(provider, m))
                .filter(|m| !self.prefs.model_hidden(&provider.provider, m))
                .map(|m| (provider, m.clone()))
                .collect();
            groups.push((provider.name.clone(), Some(provider), picks));
        }
        groups
    }

    /// The `ix`th pickable row, as ⌘1..⌘9 counts them.
    pub(crate) fn nth_pick(
        &self,
        ix: usize,
        order: &[String],
        recent: &[(String, String)],
    ) -> Option<(String, String)> {
        self.groups(order, recent)
            .into_iter()
            .flat_map(|(_, _, picks)| picks)
            .nth(ix)
            .map(|(p, m)| (p.provider.clone(), m))
    }
}

/// Bind the picker of the chat showing `id` (main or side chat) to its
/// thread and read the route. Called once per chat start.
pub fn start(s: &mut State, id: &str) {
    let Some(chat) = crate::chat::chat_for(s, id) else {
        return;
    };
    let compact = chat.model_picker.compact;
    chat.model_picker = ModelPickerState::new(Some(id.to_string()), None);
    chat.model_picker.compact = compact;
    load(s, Slot::chat(id));
}

/// ⇧⌘M: toggle the focused chat's picker. Area B wires the shortcut.
pub fn open(s: &mut State) {
    let Some(slot) = focused_slot(s) else { return };
    if let Some(p) = slot.get_mut(s)
        && (p.route.as_ref().is_some_and(|r| !r.providers.is_empty()) || p.current.is_some())
    {
        p.toggle();
        if p.open {
            refresh_catalog(s, slot);
        }
    }
}

/// The popover opened: re-read the catalog and the preferences, so a model
/// or provider hidden or removed in Settings since the chat started is gone
/// (and one added back is offered). Leaves the pick, the effort and any
/// pick in flight alone.
pub(crate) fn refresh_catalog(s: &mut State, slot: Slot) {
    let Some(p) = slot.get_mut(s) else { return };
    let thread = p.thread_id.clone();
    let generation = p.generation;
    p.prefs_seq += 1;
    let seq = p.prefs_seq;
    jobs::run(
        s,
        "model picker",
        move |c| async move {
            match thread {
                Some(id) => c.thread_route(&id).await.map(|r| (r.providers, r.picker)),
                None => c.providers().await.map(|c| (c.providers, c.picker)),
            }
        },
        move |s, result, _| {
            // silent-ok: the copy read when the chat started stays.
            let Ok((providers, prefs)) = result else {
                return;
            };
            let Some(p) = slot
                .get_mut(s)
                .filter(|p| p.generation == generation && p.prefs_seq == seq)
            else {
                return;
            };
            if let Some(route) = p.route.as_mut() {
                route.providers = providers;
            }
            p.prefs = prefs;
            if let Rail::Provider(id) = &p.rail
                && p.prefs.provider_hidden(id)
            {
                p.rail = Rail::All;
            }
            if p.prefs.favorites.is_empty() && p.rail == Rail::Favorites {
                p.rail = Rail::All;
            }
        },
    );
}

/// Read the route (catalog, preferences, the thread's pick): a chat's
/// through its thread; the new-thread screen's from `GET /providers`, which
/// needs no thread, so an account with none can still pick a model.
pub fn load(s: &mut State, slot: Slot) {
    let Some(p) = slot.get_mut(s) else { return };
    let own = p.thread_id.clone();
    let generation = p.generation.bump();
    let read = own.clone();
    let own = own.is_some();
    jobs::run(
        s,
        "model picker",
        move |c| async move {
            match read {
                Some(thread_id) => c.thread_route(&thread_id).await,
                None => c.providers().await.map(|catalog| ThreadRoute {
                    providers: catalog.providers,
                    picker: catalog.picker,
                    ..ThreadRoute::default()
                }),
            }
        },
        move |s, result, tx| route_loaded(s, &slot, generation, own, result, tx),
    );
}

/// The route `load` asked for came back: take its pick, catalog and
/// preferences. The pill appearing for the first time eases in (it asks
/// for motion; any other refresh lands as it is).
fn route_loaded(
    s: &mut State,
    slot: &Slot,
    generation: Generation,
    own: bool,
    result: Result<ThreadRoute, zlo_client::ClientError>,
    tx: &mut rune::Tx,
) {
    let recent = s.prefs.recent_models.clone();
    let Some(p) = slot.get_mut(s).filter(|p| p.generation == generation) else {
        return;
    };
    // silent-ok: an older gateway has no route; the pill stays hidden.
    let Ok(route) = result else { return };
    let hidden = !p.shows_pill();
    if own {
        p.current = route_pick(&route);
        p.effort = route.effort.clone();
    }
    p.prefs = route.picker.clone();
    if !p.prefs.favorites.is_empty() {
        p.rail = Rail::Favorites;
    }
    p.route = Some(route);
    // A draft switched to another computer keeps its explicit pick only if
    // the new computer's catalog offers that provider and model; otherwise it
    // falls back cleanly to the new computer's own default.
    if !own && let Some(((provider, model), effort)) = p.carried_pick.take() {
        let offered = !p.prefs.provider_hidden(&provider)
            && p.providers()
                .iter()
                .any(|rp| rp.provider == provider && rp.models.iter().any(|m| m == &model));
        if offered {
            let effort = effort.filter(|e| p.efforts_of(&provider, &model).contains(e));
            p.current = Some((provider, model));
            p.effort = effort;
            p.touched = true;
        } else {
            p.touched = false;
            p.effort = None;
            p.current = p.default_pick(&recent);
        }
    } else if p.current.is_none() || (!own && !p.touched) {
        // A draft re-picks once the catalog is in: the profile may have loaded first.
        p.current = p.default_pick(&recent);
    }
    if hidden && p.shows_pill() {
        tx.animated();
    }
}

/// The new-thread screen's profile: show its default model unless the user
/// already picked one.
pub fn set_profile(s: &mut State, slot: Slot, profile_id: String) {
    let Some(p) = slot.get_mut(s) else { return };
    p.profile_req = Some(profile_id.clone());
    let asked = profile_id.clone();
    jobs::run(
        s,
        "model picker",
        move |c| async move { c.profile(&profile_id).await },
        move |s, result, tx| {
            let recent = s.prefs.recent_models.clone();
            let Some(p) = slot.get_mut(s) else { return };
            // Opening the draft asks for the last-used profile, then the
            // picked one: only the latest request may set the pill.
            if p.profile_req.as_deref() != Some(asked.as_str()) {
                return;
            }
            // silent-ok: an unreadable profile leaves the recent pick on the pill.
            let Ok(info) = result else { return };
            let st = info.settings;
            p.profile_pick = st
                .default_provider
                .map(|pv| (pv, st.default_model.unwrap_or_default()));
            let hidden = !p.shows_pill();
            if !p.touched {
                p.current = p.default_pick(&recent);
            }
            // The pill's first appearance eases in, as in `route_loaded`.
            if hidden && p.shows_pill() {
                tx.animated();
            }
        },
    );
}

/// Picks the level a thinking drag let go on, a frame after the release.
/// Rune lands a whole frame at once when any of its input was a drag's
/// move, and a pointer's last move and its release usually share a frame:
/// picked there, the composer pill's level would cut instead of rolling
/// (as a model pick's name does). Its own commit, which asks for motion,
/// lets the label roll and the knob settle. True when anything changed.
pub fn poll(s: &mut State, tx: &mut rune::Tx) -> bool {
    let mut slots: Vec<Slot> = s.chat.iter().map(|c| Slot::chat(&c.thread_id)).collect();
    slots.extend(
        crate::chat::side::side_chat_ids(s)
            .iter()
            .map(|id| Slot::chat(id)),
    );
    slots.push(DRAFT);
    slots.push(crate::pages::automations::form::FORM_PICKER);
    let mut changed = false;
    for slot in slots {
        let Some(level) = slot.get_mut(s).and_then(|m| m.thinking_release.take()) else {
            continue;
        };
        tx.animated();
        changed = true;
        if let Some(m) = slot.get_mut(s) {
            m.thinking_drag = None;
            if level == m.effort {
                continue;
            }
        }
        pick_effort(s, slot, level);
    }
    changed
}

pub(crate) fn pick_effort(s: &mut State, slot: Slot, effort: Option<String>) {
    let Some(p) = slot.get_mut(s) else { return };
    let previous = std::mem::replace(&mut p.effort, effort.clone());
    p.effort_seq += 1;
    let seq = p.effort_seq;
    let Some(thread_id) = p.thread_id.clone() else {
        return;
    };
    let generation = p.generation;
    let wire = effort.unwrap_or_else(|| "default".into());
    jobs::run(
        s,
        "model picker",
        move |c| async move { c.set_thread_effort(&thread_id, &wire).await },
        move |s, result, tx| {
            if let Err(e) = result {
                if let Some(p) = slot
                    .get_mut(s)
                    .filter(|p| p.generation == generation && p.effort_seq == seq)
                {
                    // The knob glides back to the level that stuck.
                    tx.animated();
                    p.effort = previous;
                }
                slot.event(s, PickerEvent::Failed(e.to_string()));
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
    let Some(p) = slot.get_mut(s) else { return };
    p.open = false;
    p.cmd_held = false;
    p.matrix_drag = None;
    p.effort_seq += 1;
    // The effort carries over when the new model takes the same level.
    let effort = p
        .effort
        .clone()
        .filter(|e| p.efforts_of(&provider, &model).contains(e));
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
        move |c| async move {
            c.set_thread_route(&thread_id, &pv, &md, effort.as_deref())
                .await
        },
        move |s, result, tx| {
            let Some(p) = slot.get_mut(s).filter(|p| p.generation == generation) else {
                return;
            };
            // The person's pick: the pill's spinner gives way and the new
            // name rolls in (a reply lands at once otherwise).
            tx.animated();
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
                    slot.event(s, PickerEvent::Changed(change));
                }
                Err(e) => slot.event(s, PickerEvent::Failed(e.to_string())),
            }
        },
    );
}

/// Apply a preference change locally, then save it onto the gateway's
/// current preferences (re-read first, so Settings' changes survive).
/// `edit` must give the same result applied twice (`PickerPrefs::set_*`).
pub(crate) fn edit_prefs(
    s: &mut State,
    slot: Slot,
    edit: impl Fn(&mut PickerPrefs) + Send + 'static,
) {
    let Some(p) = slot.get_mut(s) else { return };
    edit(&mut p.prefs);
    p.prefs_seq += 1;
    let seq = p.prefs_seq;
    if p.prefs.favorites.is_empty() && p.rail == Rail::Favorites {
        p.rail = Rail::All;
    }
    let Some(thread_id) = p.catalog_thread.clone() else {
        return;
    };
    jobs::run(
        s,
        "model picker",
        move |c| async move { c.edit_model_picker(&thread_id, edit).await },
        move |s, result, _| match result {
            // A later edit or read owns `prefs` now; this answer is older.
            Ok(prefs) => {
                if let Some(p) = slot.get_mut(s).filter(|p| p.prefs_seq == seq) {
                    p.prefs = prefs;
                }
            }
            Err(e) => slot.event(s, PickerEvent::Failed(e.to_string())),
        },
    );
}

/// Move `dragged` into `target`'s place in the rail and the list.
pub(crate) fn reorder(s: &mut State, slot: Slot, dragged: &str, target: &str) {
    let order_pref = s.prefs.provider_order.clone();
    let Some(p) = slot.get(s) else { return };
    let mut order: Vec<String> = p
        .ordered_providers(&order_pref)
        .iter()
        .map(|p| p.provider.clone())
        .collect();
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
    let Some(p) = slot.get_mut(s) else { return };
    let mut tabs = p.rail_tabs(&order);
    let n = tabs.len();
    let at = tabs.iter().position(|r| *r == p.rail).unwrap_or(0);
    let next = if forward {
        (at + 1) % n
    } else {
        (at + n - 1) % n
    };
    p.rail = tabs.swap_remove(next);
}

/// The thread's pick as `(provider, model)`; no model is the default.
fn route_pick(route: &ThreadRoute) -> Option<(String, String)> {
    let provider = route.provider.clone().unwrap_or_else(|| NATIVE.into());
    Some((
        provider,
        route.model.clone().unwrap_or_else(|| "default".into()),
    ))
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
    if out.is_empty() {
        id.to_string()
    } else {
        out.join(" ")
    }
}

/// A picker with a small catalog, for the headless scenes.
pub fn fixture() -> ModelPickerState {
    use zlo_client::orchestration::PickerModel;
    let provider =
        |id: &str, name: &str, runtime: &str, models: &[&str], efforts: &[&str]| RouteProvider {
            provider: id.into(),
            name: name.into(),
            runtime: runtime.into(),
            authenticated: id != "openrouter",
            models: models.iter().map(|m| m.to_string()).collect(),
            efforts: efforts.iter().map(|m| m.to_string()).collect(),
            model_efforts: Default::default(),
            brand: None,
            launch: None,
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
            provider(
                "codex",
                "Codex",
                "codex",
                &["gpt-6.1-sol", "gpt-6.1-mini"],
                &["minimal", "high"],
            ),
            provider(
                "openrouter",
                "OpenRouter",
                NATIVE,
                &["deepseek/deepseek-v4"],
                &[],
            ),
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
/// with its picker's popover open, thinking on Auto (or on High).
pub fn scene(s: &mut State, effort: bool) {
    crate::scenes::chat(s);
    s.prefs.recent_models = vec![
        ("anthropic".into(), "claude-opus-5-5".into()),
        ("codex".into(), "gpt-6.1-sol".into()),
    ];
    if let Some(c) = s.chat.as_mut() {
        c.model_picker = fixture();
        if !effort {
            c.model_picker.effort = None;
        }
        c.model_picker.toggle();
    }
}

/// Richer fixture for the Signal Matrix scenes & tests: multiple connected
/// providers (Claude, Codex, Gemini) plus an unauthenticated OpenRouter entry
/// that must remain excluded.
pub fn fixture_matrix() -> ModelPickerState {
    use zlo_client::orchestration::PickerModel;
    let provider =
        |id: &str, name: &str, runtime: &str, authed: bool, models: &[&str], efforts: &[&str]| {
            RouteProvider {
                provider: id.into(),
                name: name.into(),
                runtime: runtime.into(),
                authenticated: authed,
                models: models.iter().map(|m| m.to_string()).collect(),
                efforts: efforts.iter().map(|m| m.to_string()).collect(),
                model_efforts: Default::default(),
                brand: None,
                launch: None,
            }
        };
    // Mirrors a real catalog: CLI and IronClaw providers, several hidden
    // models and providers, and models with no published score.
    let route = ThreadRoute {
        thread_id: "t1".into(),
        runtime: "claude-cli".into(),
        provider: Some("claude-cli".into()),
        model: Some("claude-opus-5-5".into()),
        effort: Some("high".into()),
        fresh: false,
        providers: vec![
            provider(
                "claude-cli",
                "Claude CLI",
                "claude-cli",
                true,
                &[
                    "claude-opus-5-5",
                    "claude-sonnet-5-5",
                    "claude-haiku-5-5",
                    "claude-fable-5-2",
                ],
                &["low", "medium", "high", "xhigh", "max"],
            ),
            provider(
                "antigravity",
                "Antigravity",
                "antigravity",
                true,
                &[
                    "gemini-4-neon",
                    "gemini-4-carbon",
                    "gemini-4-argon",
                    "gemini-4-hydrogen",
                    "gemini-pro-orchestrator",
                    "gemini-pro-orchestrator-argon",
                    "claude-opus-5-5",
                ],
                &["low", "medium", "high", "xhigh"],
            ),
            provider(
                "codex",
                "Codex",
                "codex",
                true,
                &["gpt-6.2-sol", "gpt-6.2-astra", "gpt-6.1-mini"],
                &["low", "medium", "high", "xhigh"],
            ),
            provider(
                NATIVE,
                "IronClaw",
                NATIVE,
                true,
                &["gpt-6.2-sol", "gpt-6.2-astra", "gpt-6.1-terra"],
                &["low", "medium", "high", "xhigh"],
            ),
            provider(
                "glm",
                "GLM (Z.ai coding plan)",
                NATIVE,
                true,
                &["glm-5.3-flash", "glm-5.4"],
                &["low", "medium", "high", "max"],
            ),
            provider(
                "inception",
                "Inception",
                NATIVE,
                true,
                &["mercury-3", "mercury-decide"],
                &["instant", "low", "medium", "high"],
            ),
            provider("kimi", "Kimi", NATIVE, true, &["kimi-k3.1-pro"], &[]),
            provider(
                "openrouter",
                "OpenRouter",
                NATIVE,
                false,
                &["deepseek/deepseek-v4"],
                &[],
            ),
        ],
        picker: PickerPrefs {
            hidden_providers: vec!["codex".into()],
            hidden_models: vec![
                PickerModel::new("claude-cli", "claude-sonnet-5-5"),
                PickerModel::new("claude-cli", "claude-haiku-5-5"),
                PickerModel::new("antigravity", "gemini-4-argon"),
                PickerModel::new("antigravity", "claude-opus-5-5"),
                PickerModel::new(NATIVE, "gpt-6.1-terra"),
            ],
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

/// Scene `model-picker-matrix`: open chat with Signal Matrix enabled in prefs.
pub fn scene_matrix(s: &mut State) {
    crate::scenes::chat(s);
    s.prefs.signal_matrix_picker = true;
    if let Some(c) = s.chat.as_mut() {
        c.model_picker = fixture_matrix();
        c.model_picker.toggle();
    }
}

// The new-thread screen's picker (`workspace/draft.rs`).

/// The draft screen picked a profile: show that profile's default and read
/// the catalog if it is not in yet.
pub fn draft_set_profile(s: &mut State, profile: String, catalog_thread: Option<String>) {
    let Some(d) = s.workspace.draft.as_mut() else {
        return;
    };
    // Picker preferences are saved through a thread; bind one once the
    // inbox has it, keeping any pick and catalog already made.
    if d.picker.catalog_thread.is_none() {
        d.picker.catalog_thread = catalog_thread;
    }
    // The catalog is what the pill lists: without it the pill stays hidden
    // and a new thread cannot pick its model.
    let unloaded = d.picker.route.is_none();
    set_profile(s, DRAFT, profile);
    if unloaded {
        load(s, DRAFT);
    }
}

/// Carry an explicit model/effort pick across a computer switch on the draft
/// screen. It is validated once the new computer's catalog loads (`route_loaded`)
/// so a stale model id is never sent to a backend that does not offer it.
pub fn draft_carry_pick(s: &mut State, pick: Option<(String, String)>, effort: Option<String>) {
    if let Some(d) = s.workspace.draft.as_mut() {
        d.picker.carried_pick = pick.map(|pm| (pm, effort));
    }
}

/// The draft screen's model pill.
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

#[cfg(test)]
mod tests {
    use super::{effort_label, pretty_model};

    /// The pill shows up once the route comes back: that first appearance
    /// eases in (the reply asks for motion), and a later refresh, with the
    /// pill already there, lands as it is.
    #[test]
    fn the_pill_eases_in_when_the_route_first_loads() {
        use rune::{Env, Runtime, Tx};
        let mut s = crate::chat::fixtures::test_state();
        super::scene(&mut s, false);
        let slot = super::Slot::chat("t1");
        let (route, generation) = {
            let m = slot.get_mut(&mut s).expect("picker");
            m.open = false;
            m.current = None;
            (m.route.take().expect("fixture route"), m.generation)
        };
        let mut rt = Runtime::new(s, crate::view, (1440., 900.), Env::new(true, 1.));
        for _ in 0..30 {
            rt.frame(1. / 60.);
        }
        let key = slot.key("model-pills");
        assert!(rt.presented(&key).is_none(), "no pill before the route");
        let first = route.clone();
        rt.update(|s, tx| {
            super::route_loaded(s, &slot, generation, true, Ok(first), tx);
        });
        rt.frame(1. / 60.);
        rt.frame(1. / 60.);
        let pill = rt
            .elements()
            .into_iter()
            .find(|e| e.key.as_deref() == Some(key.as_str()))
            .expect("pill drawn");
        assert!(
            pill.opacity < 0.99 && pill.scale < 0.999,
            "the pill eases in: opacity {}, scale {}",
            pill.opacity,
            pill.scale
        );

        let mut tx = Tx::default();
        super::route_loaded(&mut rt.state, &slot, generation, true, Ok(route), &mut tx);
        assert!(
            !crate::workspace::asked_motion(&tx),
            "a refresh with the pill shown lands as it is"
        );
    }

    /// In a thread the panel opens above the composer and clears the whole
    /// card: hung off the pill alone, it covered the text being typed.
    #[test]
    fn the_panel_opens_clear_of_the_composer() {
        use rune::{Env, Runtime};
        let mut s = crate::chat::fixtures::test_state();
        super::scene(&mut s, false);
        let mut rt = Runtime::new(s, crate::view, (1440., 900.), Env::new(true, 1.));
        let mut t = 0.;
        for _ in 0..60 {
            t += 1. / 60.;
            rt.frame(t);
        }
        let (panel, _) = rt.presented("model-popover").expect("panel drawn");
        let (card, _) = rt.presented("composer:t1").expect("composer drawn");
        assert!(
            panel[1] + panel[3] <= card[1],
            "the panel (bottom {:.1}) overlaps the composer (top {:.1})",
            panel[1] + panel[3],
            card[1]
        );
    }

    /// One control: the pill carries the thinking level and its popover
    /// holds the levels under the models; there is no second pill.
    #[test]
    fn model_and_thinking_are_one_pill_and_one_popover() {
        use rune::{Env, Runtime};
        let mut s = crate::chat::fixtures::test_state();
        super::scene(&mut s, true);
        let mut rt = Runtime::new(s, crate::view, (1440., 900.), Env::new(true, 1.));
        let mut t = 0.;
        for _ in 0..60 {
            t += 1. / 60.;
            rt.frame(t);
        }
        assert!(rt.presented("model-pill:t1").is_some(), "the pill is drawn");
        assert!(
            rt.presented("effort-pill:t1").is_none(),
            "no separate thinking pill"
        );
        let (popover, _) = rt.presented("model-popover").expect("panel drawn");
        let (row, _) = rt.presented("model-thinking").expect("thinking row drawn");
        assert!(
            row[1] >= popover[1] && row[1] + row[3] <= popover[1] + popover[3] + 0.5,
            "the thinking row sits inside the popover"
        );
        for level in ["auto", "low", "medium", "high", "xhigh"] {
            assert!(
                rt.presented(&format!("thinking-{level}")).is_some(),
                "level {level} offered"
            );
        }
    }

    /// A removed provider and a hidden model are out of the picker with no
    /// way back from it: no rail tab, no group, no row (they return from
    /// Settings → Providers).
    #[test]
    fn hidden_and_removed_stay_out_of_the_picker() {
        use super::{Rail, fixture};
        let mut p = fixture();
        p.prefs.hidden_providers.push("openrouter".into());
        let rows: Vec<(String, String)> = p
            .groups(&[], &[])
            .into_iter()
            .flat_map(|(_, _, picks)| picks)
            .map(|(pv, m)| (pv.provider.clone(), m))
            .collect();
        assert!(
            !rows.iter().any(|(pv, _)| pv == "openrouter"),
            "removed provider listed"
        );
        assert!(
            !rows
                .iter()
                .any(|(pv, m)| pv == "codex" && m == "gpt-6.1-mini"),
            "hidden model listed"
        );
        assert!(
            rows.iter()
                .any(|(pv, m)| pv == "codex" && m == "gpt-6.1-sol")
        );
        assert!(
            !p.rail_tabs(&[])
                .contains(&Rail::Provider("openrouter".into()))
        );
        // A favorite of a removed provider is not offered either.
        p.prefs
            .set_favorite("openrouter", "deepseek/deepseek-v4", true);
        p.rail = Rail::Favorites;
        assert!(p.groups(&[], &[])[0].2.is_empty());
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
        assert_eq!(
            pretty_model("anthropic/claude-haiku-5-5"),
            "Claude Haiku 5.5"
        );
    }
}
