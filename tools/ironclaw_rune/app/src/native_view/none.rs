//! No native views on this system: [`Surface::create`] never makes one, so
//! the pane keeps its fallbacks (`super::shows_natively` is false and the
//! Browser tab sends pages to the system browser).

use std::convert::Infallible;
use std::time::Instant;

use super::{Engine, PageChange, Slot, Source, WebCommand, WebState};

/// Native views do not exist here.
pub(super) const AVAILABLE: bool = false;

/// No window view to host native views in.
pub(super) type Host = Infallible;

pub(super) fn host_of(_: &rune::WindowHandle) -> Result<Option<Host>, String> {
    Ok(None)
}

pub(super) fn same_host(a: &Host, _: &Host) -> bool {
    match *a {}
}

pub(super) fn install_input_monitor() {}

pub(super) fn keep_waker(_: &rune::AppHandle<crate::state::State>) {}

pub(super) fn take_page_change(_: Instant) -> PageChange {
    PageChange::None
}

#[cfg(test)]
pub(super) fn note_page_change_for_tests() -> bool {
    false
}

/// The system's reduce-motion setting is not read here.
pub(super) fn system_reduced_motion() -> bool {
    false
}

/// Never made: [`Surface::create`] has no host to make it in.
pub(super) struct Surface(Infallible);

impl Surface {
    pub(super) fn create(_: Slot, host: Host, _: &Source) -> Option<Self> {
        match host {}
    }

    pub(super) fn host(&self) -> &Host {
        &self.0
    }

    pub(super) fn engine(&self) -> Engine {
        match self.0 {}
    }

    pub(super) fn source(&self) -> &Source {
        match self.0 {}
    }

    pub(super) fn hidden_since(&self) -> Option<Instant> {
        match self.0 {}
    }

    pub(super) fn load(&mut self, _: &Source) {
        match self.0 {}
    }

    pub(super) fn place(&mut self, _: [f32; 4]) {
        match self.0 {}
    }

    pub(super) fn set_hidden(&mut self, _: bool, _: impl FnOnce() -> bool) {
        match self.0 {}
    }

    pub(super) fn destroy(self) {
        match self.0 {}
    }

    pub(super) fn refresh(&self) {
        match self.0 {}
    }

    pub(super) fn web_state(&self) -> WebState {
        match self.0 {}
    }

    pub(super) fn web(&self, _: WebCommand) {
        match self.0 {}
    }
}
