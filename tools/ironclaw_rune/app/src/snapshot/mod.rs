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

// PORT-STUB(area F): register the SnapShot hotkeys at launch (GPUI
// `snapshot::start`); a capture lands through `workspace::stage_snapshot`.
pub fn start(_s: &mut State) {}

// PORT-STUB(area F): a captured screen (GPUI `snapshot::Snapshot`).
pub struct Snapshot {}

// PORT-STUB(area F): stage a snapshot's picture and AX text as files (GPUI
// `snapshot::stage_into`, without its `cx`).
pub fn stage_into(
    _staged: &mut Vec<ironclaw_gpui_client::attachments::StagedFile>,
    _snap: Snapshot,
    _limits: &ironclaw_gpui_client::wire::AttachmentLimits,
) -> Result<(), String> {
    Err("SnapShot is not ported yet.".into())
}
