//! How each skill is listed to the models: in full, by name only (still
//! callable, almost free), or not at all. ZLO decides from how often a skill
//! is used; a pin overrides it for every model (ZLO's own and each CLI).

use rune::{El, col, row, text};
use rune_kit::Kit;
use zlo_client::skill_listing::{
    RebornSkillListingEntry, RebornSkillListingResponse, RebornSkillListingState,
};

use crate::jobs;
use crate::pages::common::controls::rule;
use crate::pages::common::ts::{sm_medium, xs};
use crate::pages::common::{card, segmented};
use crate::state::State;

use super::st;

const CHOICES: [&str; 4] = ["Auto", "Full", "Name only", "Off"];

pub fn refresh(s: &mut State) {
    jobs::run(
        s,
        "skills",
        |c| async move { c.skill_listing().await },
        |s, result, _| match result {
            Ok(listing) => st(s).listing = Some(listing),
            // An older backend has no listing: the page shows none.
            Err(e) => tracing::warn!(error = %e, "skill listing not read"),
        },
    );
}

fn set_pin(s: &mut State, name: String, pin: Option<RebornSkillListingState>) {
    st(s).pinning = Some(name.clone());
    jobs::run(
        s,
        "skills",
        move |c| async move { c.set_skill_listing_pin(&name, pin).await },
        |s, result, _| {
            let p = st(s);
            p.pinning = None;
            match result {
                // The stored listing, read back by the gateway.
                Ok(listing) => p.listing = Some(listing),
                Err(e) => p.error = Some(e.to_string()),
            }
        },
    );
}

fn entry<'a>(s: &'a State, name: &str) -> Option<&'a RebornSkillListingEntry> {
    s.pages
        .skills
        .listing
        .as_ref()
        .filter(|l| l.managed)?
        .skills
        .iter()
        .find(|e| e.name == name)
}

/// "Name only · auto · 3 uses · in ZLO, codex".
fn summary(e: &RebornSkillListingEntry) -> String {
    let state = match e.listing {
        RebornSkillListingState::Full => "Full",
        RebornSkillListingState::NameOnly => "Name only",
        RebornSkillListingState::Off => "Off",
    };
    // A pinned skill is listed as its pin; otherwise usage decided.
    let basis = if e.pin.is_some() { "pinned" } else { "auto" };
    let uses = match e.uses {
        0 => "never used".to_string(),
        1 => "1 use".to_string(),
        n => format!("{n} uses"),
    };
    let mut parts = vec![state.to_string(), basis.to_string(), uses];
    let sources: Vec<&str> = e
        .sources
        .iter()
        .map(|x| match x.as_str() {
            "harness" => "ZLO",
            other => other.strip_prefix("cli:").unwrap_or(other),
        })
        .collect();
    if !sources.is_empty() {
        parts.push(format!("in {}", sources.join(", ")));
    }
    parts.join(" · ")
}

fn picked(e: &RebornSkillListingEntry) -> usize {
    match e.pin {
        None => 0,
        Some(RebornSkillListingState::Full) => 1,
        Some(RebornSkillListingState::NameOnly) => 2,
        Some(RebornSkillListingState::Off) => 3,
    }
}

/// The listing line and pin control for one skill; `None` when the backend
/// does not manage listings or the skill was never offered to a model.
pub fn control(s: &State, k: &Kit, name: &str) -> Option<El> {
    let p = &s.palette;
    let e = entry(s, name)?;
    let busy = s.pages.skills.pinning.as_deref() == Some(name);
    let owned = name.to_string();
    Some(
        row()
            .w_full()
            .gap(8.)
            .center_y()
            .child(
                text(summary(e))
                    .font(xs())
                    .color(p.text_muted)
                    .grow(1.)
                    .min_w(0.)
                    .wrap(),
            )
            .child(
                segmented(k, &CHOICES, picked(e), busy, move |s, ix, _| {
                    let pin = match ix {
                        1 => Some(RebornSkillListingState::Full),
                        2 => Some(RebornSkillListingState::NameOnly),
                        3 => Some(RebornSkillListingState::Off),
                        _ => None,
                    };
                    set_pin(s, owned.clone(), pin);
                })
                .key(format!("listing-{name}")),
            ),
    )
}

/// Skills a CLI agent offers that are not installed in ZLO (its own
/// bundled or account skills), with the same control.
pub fn others(s: &State, k: &Kit) -> Option<El> {
    let p = &s.palette;
    let sk = &s.pages.skills;
    let listing: &RebornSkillListingResponse = sk.listing.as_ref().filter(|l| l.managed)?;
    let rows: Vec<El> = listing
        .skills
        .iter()
        .filter(|e| !sk.skills.iter().any(|x| x.name == e.name))
        .filter_map(|e| {
            Some(
                col()
                    .key(format!("other-skill-{}", e.name))
                    .w_full()
                    .gap(4.)
                    .py(6.)
                    .child(text(e.name.clone()).font(sm_medium()).color(p.text))
                    .child(control(s, k, &e.name)?),
            )
        })
        .collect();
    if rows.is_empty() {
        return None;
    }
    let mut list = col().w_full().gap(4.);
    for (ix, r) in rows.into_iter().enumerate() {
        if ix > 0 {
            list = list.child(rule(p));
        }
        list = list.child(r);
    }
    Some(
        card(p)
            .key("other-skills")
            .child(
                col()
                    .w_full()
                    .gap(2.)
                    .child(text("From CLI agents").font(sm_medium()).color(p.text))
                    .child(
                        text("Skills the coding agents bring themselves. Rarely used ones are listed by name only, so they cost almost nothing but can still be called.")
                            .font(xs())
                            .color(p.text_muted)
                            .wrap(),
                    ),
            )
            .child(list),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The Skills scene's listing decodes from the wire shape, and a
    /// pinned skill reads as pinned from its pin alone.
    #[test]
    fn the_scene_listing_decodes_and_reads_its_pins() {
        let mut s = crate::chat::fixtures::test_state();
        crate::pages::skills::fixtures::skills(&mut s);
        let pinned = entry(&s, "rust-review").expect("the scene's listing decodes");
        assert_eq!(pinned.pin, Some(RebornSkillListingState::NameOnly));
        assert_eq!(picked(pinned), 2);
        assert!(
            summary(pinned).starts_with("Name only · pinned"),
            "{}",
            summary(pinned)
        );
        let auto = entry(&s, "commit").expect("commit listed");
        assert_eq!(summary(auto), "Full · auto · 14 uses · in ZLO, claude-cli");
    }
}
