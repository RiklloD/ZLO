//! Stop, Take over and Hand back: the control requests and their replies.

use super::*;

pub(super) fn control(s: &mut State, id: &str, action: ComputerControl) {
    let Some(cu) = slot(s, id) else { return };
    if cu.busy {
        return;
    }
    cu.busy = true;
    cu.note = None;
    let (thread_id, viewer, generation) = (cu.thread_id.clone(), cu.viewer.clone(), cu.generation);
    let (asked, owner) = (thread_id.clone(), viewer.clone());
    jobs::run(
        s,
        "computer use",
        move |c| async move { c.computer_control(&thread_id, action, &viewer).await },
        move |s, result, tx| {
            // The reply to the person's Take over / Hand back / Stop click:
            // the pause scrim and the button's fill move as its answer.
            tx.animated();
            control_landed(s, &asked, generation, owner, action, result.map(|_| ()))
        },
    );
}

/// A control request's answer. A Take Over that succeeded after the share
/// that asked went away (its chat closed, or reopened as a new share) has no
/// owner to hand it back on drop, so it is handed back at once.
pub(super) fn control_landed(
    s: &mut State,
    asked: &str,
    generation: Generation,
    viewer: String,
    action: ComputerControl,
    result: Result<(), zlo_client::ClientError>,
) {
    let Some(cu) = same(s, asked, generation) else {
        if result.is_ok() && matches!(action, ComputerControl::TakeOver) {
            tracing::debug!("take over landed for a closed share; handing back");
            hand_back(s, asked.to_string(), viewer);
        }
        return;
    };
    cu.busy = false;
    match result {
        Ok(()) => {
            match action {
                ComputerControl::TakeOver => cu.mine = true,
                ComputerControl::HandBack | ComputerControl::Stop => cu.mine = false,
            }
            sync_cards(s, asked);
        }
        Err(e) => {
            cu.note = Some(
                match action {
                    ComputerControl::Stop => "Could not stop the agent",
                    ComputerControl::TakeOver => "Could not take over",
                    ComputerControl::HandBack => "Could not hand back",
                }
                .into(),
            );
            tracing::warn!(error = %e, "computer-use control failed");
        }
    }
}

#[cfg(test)]
thread_local! {
    pub(super) static HANDED_BACK: std::cell::RefCell<Vec<(String, String)>> = const { std::cell::RefCell::new(Vec::new()) };
}

/// Give the desktop `viewer` holds in `thread_id` back to the agent.
pub(super) fn hand_back(s: &State, thread_id: String, viewer: String) {
    #[cfg(test)]
    HANDED_BACK.with(|h| h.borrow_mut().push((thread_id.clone(), viewer.clone())));
    jobs::run(
        s,
        "computer use",
        move |c| async move {
            c.computer_control(&thread_id, ComputerControl::HandBack, &viewer)
                .await
        },
        |_, _, _| {},
    );
}

pub(super) fn stop(s: &mut State, id: &str) {
    control(s, id, ComputerControl::Stop);
    // The Hermes viewer also interrupts the whole turn.
    crate::chat::stop_run(s, id);
}

pub(super) fn take_over_or_back(s: &mut State, id: &str) {
    let Some(cu) = slot(s, id) else { return };
    let action = if cu.mine {
        ComputerControl::HandBack
    } else {
        ComputerControl::TakeOver
    };
    control(s, id, action);
}
