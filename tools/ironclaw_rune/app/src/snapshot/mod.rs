//! Screen snapshots sent to the agent (global hotkey, capture, AX tree).
//!
//! FOUNDATION STUB. The snapshot port (PLAN.md, area F) replaces this.

use rune::Tx;

use crate::state::State;

#[derive(Default)]
pub struct SnapshotState {}

pub fn poll(_s: &mut State, _tx: &mut Tx) -> bool {
    false
}
