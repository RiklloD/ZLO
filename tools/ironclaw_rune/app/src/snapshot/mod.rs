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

// ---- Called by the chat (area A). PORT-STUB(area F): replace with the
// snapshot port; keep the signatures.

use ironclaw_gpui_client::attachments::StagedFile;
use ironclaw_gpui_client::wire::AttachmentLimits;

const IMAGE_SUFFIX: &str = ".snapshot.jpg";
const TEXT_SUFFIX: &str = ".snapshot.txt";

/// PORT-STUB(area F): a captured screen (GPUI `snapshot::Snapshot`).
pub struct Snapshot {}

/// PORT-STUB(area F): stage `snap` as one picture and its text part, within
/// the limits; both or neither.
pub fn stage_into(
    _s: &mut State,
    _staged: &mut Vec<StagedFile>,
    _snap: Snapshot,
    _limits: &AttachmentLimits,
) -> Result<(), String> {
    Err("SnapShot is not ported yet.".into())
}

/// The text half of a SnapShot staged with its picture: it has no pill of
/// its own. A `.snapshot.txt` picked by hand keeps its plain pill.
pub fn is_companion(file: &StagedFile, staged: &[StagedFile]) -> bool {
    file.filename.strip_suffix(TEXT_SUFFIX).is_some_and(|stem| {
        let image = format!("{stem}{IMAGE_SUFFIX}");
        staged.iter().any(|f| f.filename == image)
    })
}

/// Whether `text` is the text half of the SnapShot picture `image`.
pub fn pairs(text: &StagedFile, image: &StagedFile) -> bool {
    match (
        text.filename.strip_suffix(TEXT_SUFFIX),
        image.filename.strip_suffix(IMAGE_SUFFIX),
    ) {
        (Some(a), Some(b)) => a == b,
        _ => false,
    }
}

/// PORT-STUB(area F): a SnapShot picture's app name for its pill, and the
/// spoken label that adds the window title.
pub fn names(_s: &State, _file: &StagedFile) -> Option<(String, String)> {
    None
}

/// PORT-STUB(area F): whether `file` is a SnapShot picture this app took
/// (its pill shows the frame icon).
pub fn is_picture(_s: &State, file: &StagedFile) -> bool {
    file.filename.ends_with(IMAGE_SUFFIX)
}
