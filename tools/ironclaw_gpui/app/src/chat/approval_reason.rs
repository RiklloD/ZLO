//! "Why the agent asked": the reason behind an action, in the agent's words.
//! An approval card shows it as one muted line under the request; a tool or
//! settled approval in the activity tree opens to it, so a call that ran
//! without asking (auto-approve) still says why it ran. The reason itself
//! comes from the transcript (`ironclaw_gpui_client::transcript`).

use crate::design::Palette;
use crate::ui::*;

use super::flow;

const LABEL: &str = "Why: ";

/// The muted line under an approval card's request, when there is a reason.
pub(super) fn gate_line(reason: Option<&str>, p: &Palette) -> Option<AnyElement> {
    let reason = shown(reason)?;
    Some(
        div()
            .pl(px(28.))
            .text_size(flow::ui(11.5))
            .line_height(px(17.))
            .text_color(p.text_muted)
            .whitespace_normal()
            .child(format!("{LABEL}{reason}"))
            .into_any_element(),
    )
}

/// An activity row's opened text with the reason on top, so a row with a
/// reason can always be opened to read it.
pub(super) fn leaf_body(reason: Option<&str>, body: Option<String>) -> Option<String> {
    match (shown(reason), body) {
        (Some(reason), Some(body)) => Some(format!("{LABEL}{reason}\n\n{body}")),
        (Some(reason), None) => Some(format!("{LABEL}{reason}")),
        (None, body) => body,
    }
}

fn shown(reason: Option<&str>) -> Option<&str> {
    reason.map(str::trim).filter(|r| !r.is_empty())
}
