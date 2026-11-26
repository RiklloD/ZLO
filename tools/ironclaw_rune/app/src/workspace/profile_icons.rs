//! Profile logos, as t3code shows a project's favicon in place of its
//! monogram: a profile whose folder has a logo lists an `icon_revision`,
//! the bytes come from that profile's own computer (`profiles.icon`), and
//! the badge draws the image once it has decoded; until then, or when it
//! fails, the letters.
//!
//! Every computer's profile list (the focused one and each ready peer) is
//! reconciled from the app's poll ([`sync`]): a new or changed revision is
//! fetched once, a profile without one drops its image.
//!
//! Public API:
//! - [`start`]`(s)` once, before the first frame (gives the image cache).
//! - [`sync`]`(s)` from the poll.
//! - [`image`]`(s, computer, profile_id)` for the badge.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use base64::Engine as _;
use rune::{Image, ImageCache};
use zlo_client::ClientError;
use zlo_client::inbox::InboxProfile;

use crate::state::State;

/// The size logos rasterize at (points), at 2× for Retina; the badge draws
/// smaller and samples the mips.
const RASTER_PT: f32 = 16.;
const RASTER_SCALE: f32 = 2.;
/// A request whose answer never landed (the computer changed route or
/// went away) is asked again after this.
const RETRY_AFTER: Duration = Duration::from_secs(60);

#[derive(Default)]
pub struct ProfileIcons {
    cache: Option<ImageCache>,
    /// (computer key, profile id) → its logo.
    icons: HashMap<(String, String), Icon>,
}

struct Icon {
    revision: String,
    image: Option<Image>,
    /// When the bytes were asked for; `None` once answered.
    asked: Option<Instant>,
}

/// Once, from `App::init`, with a waker that redraws when a logo decodes.
pub fn start(s: &mut State) {
    let handle = s.rt.handle.clone();
    s.workspace.profile_icons.cache = Some(ImageCache::new(Arc::new(move || handle.wake())));
}

/// The decoded logo of `profile_id` on `computer`, if it has one.
pub(in crate::workspace) fn image(s: &State, computer: &str, profile_id: &str) -> Option<Image> {
    let icon = s
        .workspace
        .profile_icons
        .icons
        .get(&(computer.to_string(), profile_id.to_string()))?;
    icon.image
        .as_ref()
        .filter(|image| image.is_ready() && image.error().is_none())
        .cloned()
}

/// Reconcile every listed profile's logo with its revision: fetch new and
/// changed ones, forget those that are gone.
pub fn sync(s: &mut State) {
    if s.fixture {
        return;
    }
    let focused = s.onboarding.computer.key();
    let mut wanted: Vec<(String, bool, String, String)> = Vec::new();
    let mut listed = |computer: &str, is_focused: bool, profiles: &[InboxProfile]| {
        for p in profiles {
            if let Some(rev) = &p.icon_revision {
                wanted.push((
                    computer.to_string(),
                    is_focused,
                    p.profile_id.clone(),
                    rev.clone(),
                ));
            }
        }
    };
    listed(&focused, true, &s.workspace.inbox.list.profiles);
    let mut computers = vec![focused.clone()];
    for peer in crate::remote::peers::list(s) {
        let key = peer.computer.key();
        computers.push(key.clone());
        if let Some(inbox) = &peer.inbox {
            listed(&key, false, &inbox.profiles);
        }
    }

    // A computer still in use that no longer lists a logo drops it; one
    // gone from view keeps what it had, for when it is back.
    let icons = &mut s.workspace.profile_icons.icons;
    icons.retain(|(computer, id), _| {
        !computers.contains(computer) || wanted.iter().any(|(c, _, p, _)| c == computer && p == id)
    });

    let now = Instant::now();
    for (computer, is_focused, profile_id, revision) in wanted {
        let key = (computer.clone(), profile_id.clone());
        let due = match s.workspace.profile_icons.icons.get(&key) {
            None => true,
            Some(icon) if icon.revision != revision => true,
            Some(icon) => icon
                .asked
                .is_some_and(|at| now.duration_since(at) > RETRY_AFTER),
        };
        if due {
            fetch(s, computer, is_focused, profile_id, revision, now);
        }
    }
}

/// A headless scene's logo for `profile_id` on the focused computer,
/// decoded before the frame is drawn (scenes never fetch).
pub fn fixture_logo(s: &mut State, profile_id: &str, mime: &str, bytes: &[u8]) {
    let Some(cache) = s.workspace.profile_icons.cache.as_ref() else {
        return;
    };
    let image = cache.load_mime(bytes.to_vec(), mime, RASTER_PT, RASTER_SCALE);
    if let Err(e) = image.wait() {
        tracing::warn!("fixture profile logo failed to decode: {e:?}");
    }
    let key = (s.onboarding.computer.key(), profile_id.to_string());
    s.workspace.profile_icons.icons.insert(
        key,
        Icon {
            revision: "fixture".into(),
            image: Some(image),
            asked: None,
        },
    );
}

fn fetch(
    s: &mut State,
    computer: String,
    is_focused: bool,
    profile_id: String,
    revision: String,
    now: Instant,
) {
    let key = (computer.clone(), profile_id.clone());
    s.workspace.profile_icons.icons.insert(
        key,
        Icon {
            revision: revision.clone(),
            image: None,
            asked: Some(now),
        },
    );
    let id = profile_id.clone();
    let ask = move |c: zlo_client::Client| async move {
        let icon = c.profile_icon(&id).await?;
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(icon.data_base64.as_bytes())
            .map_err(|e| ClientError::Decode(format!("profile icon: {e}")))?;
        Ok::<_, ClientError>((icon.mime, bytes))
    };
    let landed_computer = computer.clone();
    let done =
        move |s: &mut State, result: Result<(String, Vec<u8>), ClientError>, _: &mut rune::Tx| {
            land(s, landed_computer, profile_id, &revision, result);
        };
    if is_focused {
        crate::jobs::run(s, "profile icon", ask, done);
    } else {
        crate::remote::peers::run(s, &computer, "profile icon", ask, done);
    }
}

fn land(
    s: &mut State,
    computer: String,
    profile_id: String,
    revision: &str,
    result: Result<(String, Vec<u8>), ClientError>,
) {
    let icons = &mut s.workspace.profile_icons;
    let Some(icon) = icons.icons.get_mut(&(computer, profile_id)) else {
        return;
    };
    // A newer revision was asked for since: this answer is the old file.
    if icon.revision != revision {
        return;
    }
    icon.asked = None;
    match (result, icons.cache.as_ref()) {
        (Ok((mime, bytes)), Some(cache)) => {
            icon.image = Some(cache.load_mime(bytes, &mime, RASTER_PT, RASTER_SCALE));
        }
        // An older gateway (404) or a logo that went away: letters, until
        // the revision changes.
        (Err(e), _) => tracing::debug!(error = %e, "profile icon not fetched"),
        (Ok(_), None) => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn insert(s: &mut State, computer: &str, id: &str, rev: &str, asked: Option<Instant>) {
        s.workspace.profile_icons.icons.insert(
            (computer.into(), id.into()),
            Icon {
                revision: rev.into(),
                image: None,
                asked,
            },
        );
    }

    #[test]
    fn an_answer_for_an_older_revision_is_dropped() {
        let mut s = crate::chat::fixtures::test_state();
        start(&mut s);
        insert(&mut s, "mac", "site", "r2", Some(Instant::now()));
        land(
            &mut s,
            "mac".into(),
            "site".into(),
            "r1",
            Ok(("image/svg+xml".into(), b"<svg/>".to_vec())),
        );
        let icon = &s.workspace.profile_icons.icons[&("mac".into(), "site".into())];
        assert!(icon.image.is_none(), "r1's bytes never stand for r2");
        assert!(icon.asked.is_some(), "r2 is still being asked for");
    }

    #[test]
    fn a_landed_logo_decodes_and_draws() {
        let mut s = crate::chat::fixtures::test_state();
        start(&mut s);
        insert(&mut s, "mac", "site", "r1", Some(Instant::now()));
        let svg = br##"<svg xmlns="http://www.w3.org/2000/svg" width="16" height="16"><rect width="16" height="16" fill="#f00"/></svg>"##;
        land(
            &mut s,
            "mac".into(),
            "site".into(),
            "r1",
            Ok(("image/svg+xml".into(), svg.to_vec())),
        );
        let icon = &s.workspace.profile_icons.icons[&("mac".into(), "site".into())];
        assert!(icon.asked.is_none());
        icon.image
            .as_ref()
            .expect("an image")
            .wait()
            .expect("it decodes");
        assert!(image(&s, "mac", "site").is_some());
        assert!(
            image(&s, "other", "site").is_none(),
            "logos are per computer"
        );
    }

    #[test]
    fn a_failed_fetch_keeps_the_letters() {
        let mut s = crate::chat::fixtures::test_state();
        start(&mut s);
        insert(&mut s, "mac", "site", "r1", Some(Instant::now()));
        land(
            &mut s,
            "mac".into(),
            "site".into(),
            "r1",
            Err(ClientError::Config("404".into())),
        );
        assert!(image(&s, "mac", "site").is_none());
        let icon = &s.workspace.profile_icons.icons[&("mac".into(), "site".into())];
        assert!(
            icon.asked.is_none(),
            "not asked again until the revision changes"
        );
    }
}
