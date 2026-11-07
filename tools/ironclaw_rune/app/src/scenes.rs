//! Headless scenes: named fixture states for `--snap out.png --scene NAME`
//! (and `--tree`, `--script`). Every area adds scenes for its views here, so
//! each port is checked by rendering, not only by compiling. A scene sets
//! `s.conn = Conn::Ready` and fills the state with fixture data; no gateway
//! is needed. Keep fixture builders in the owning module
//! (e.g. `chat::fixtures`) and list them here, one line each, sorted.

use crate::design::gallery;
use crate::state::{Conn, Page, State};

pub const SCENES: &[(&str, fn(&mut State))] = &[
    ("kit-gallery", gallery::fixture_kit_gallery),
    ("kit-gallery-light", gallery::fixture_kit_gallery_light),
    ("look-wallpaper-ascii", gallery::fixture_wallpaper_ascii),
    ("look-wallpaper-dither", gallery::fixture_wallpaper_dither),
    ("look-wallpaper-dither-full", gallery::fixture_wallpaper_dither_full),
    ("look-wallpaper-halftone", gallery::fixture_wallpaper_halftone),
    ("look-wallpaper-scanlines", gallery::fixture_wallpaper_scanlines),
    ("look-wallpaper-scene", gallery::fixture_wallpaper_scene),
    ("shell", shell),
];

/// The window with a few threads and nothing open.
fn shell(s: &mut State) {
    s.fixture = true;
    s.conn = Conn::Ready;
    s.page = Page::Chat;
    s.workspace.threads = vec![
        ("t1".into(), "Port the frontend to Rune".into()),
        ("t2".into(), "Fix the flaky integration test".into()),
    ];
}
