//! Area G's headless fixtures (`scenes.rs`): the wallpaper over a mock shell
//! and a gallery of every shared widget, in both schemes. The workspace
//! shell does not place the wallpaper or use the kit yet (areas B and the
//! pages adopt them at integration), so these scenes render their own
//! preview view through [`preview`], which `main.rs` consults first; only
//! fixtures ever set `LookState::preview`.

use rune::tok::TextStyle;
use rune::{Cx, El, col, el, row, text};

use super::kit::{self, Button};
use super::prefs::Wallpaper;
use super::styles::Style;
use super::{brands, panels};
use crate::icons;
use crate::state::{Conn, State};

/// What a fixture shows in place of the workspace.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Preview {
    /// A mock shell with the wallpaper over the window.
    Shell,
    /// The widget gallery.
    Gallery,
}

const SIZE: (u32, u32) = (1280, 820);

fn base(s: &mut State, preview: Preview) {
    s.fixture = true;
    s.conn = Conn::Ready;
    s.look.preview = Some(preview);
}

fn scheme(s: &mut State, light: bool) {
    s.prefs.scheme = if light { "light" } else { "dark" }.into();
    s.palette = super::resolve(&s.prefs, !light);
}

fn wallpaper(s: &mut State, name: &str, effect: &str, opacity: f32, light: bool) {
    base(s, Preview::Shell);
    scheme(s, light);
    s.prefs.wallpaper = Wallpaper::Scene { name: name.into() };
    s.prefs.wallpaper_effect = effect.into();
    s.prefs.wallpaper_area = "window".into();
    s.prefs.wallpaper_opacity = opacity;
    super::look::fixture_wallpaper(s, SIZE);
}

/// The auto scene (Dusk on dark) painted plainly, at full strength.
pub fn fixture_wallpaper_scene(s: &mut State) {
    wallpaper(s, "auto", "none", 1., false);
}

/// The default look: Dusk, dithered, at 20 % over the shell.
pub fn fixture_wallpaper_dither(s: &mut State) {
    wallpaper(s, "auto", "dither", 0.2, false);
}

/// The dither at full strength, to judge the dots.
pub fn fixture_wallpaper_dither_full(s: &mut State) {
    wallpaper(s, "night", "dither", 1., false);
}

/// Sky on the light scheme through the halftone, at full strength.
pub fn fixture_wallpaper_halftone(s: &mut State) {
    wallpaper(s, "auto", "halftone", 1., true);
}

/// Night through the scanlines, at full strength.
pub fn fixture_wallpaper_scanlines(s: &mut State) {
    wallpaper(s, "night", "scanlines", 1., false);
}

/// Dusk through the ASCII effect, at full strength.
pub fn fixture_wallpaper_ascii(s: &mut State) {
    wallpaper(s, "dusk", "ascii", 1., false);
}

const LOGOS: [&str; 10] = [
    "anthropic", "openai", "gemini", "mistral", "groq", "ollama", "antigravity", "claude-cli-2",
    "nearai", "deepseek",
];

/// Every shared widget on the dark scheme.
pub fn fixture_kit_gallery(s: &mut State) {
    base(s, Preview::Gallery);
    scheme(s, false);
    brands::preload(s, &LOGOS, 20.);
}

/// Every shared widget on the light scheme.
pub fn fixture_kit_gallery_light(s: &mut State) {
    base(s, Preview::Gallery);
    scheme(s, true);
    brands::preload(s, &LOGOS, 20.);
}

/// The fixture's view, or `None` outside fixtures.
pub fn preview(s: &State, _cx: &mut Cx<State>) -> Option<El> {
    let body = match s.look.preview? {
        Preview::Shell => shell(s),
        Preview::Gallery => gallery(s),
    };
    Some(rune_kit::root(body, Vec::new()))
}

fn label(s: &State, t: &str, size: f32, muted: bool) -> El {
    let p = &s.palette;
    text(t.to_string())
        .font(TextStyle::new(size, 400.))
        .color(if muted { p.text_muted } else { p.text })
}

/// A mock of the shell (sidebar, panel, composer) with the wallpaper
/// placed as area B will place it: last child of the window.
fn shell(s: &State) -> El {
    let p = &s.palette;
    let st = Style::of(&s.prefs);
    let mut side = col().w(super::SIDEBAR_WIDTH).h_full().padding(10.).gap(2.).pt(46.);
    for (i, t) in ["Port the frontend to Rune", "Fix the flaky integration test", "Wallpaper effects"]
        .iter()
        .enumerate()
    {
        let r = kit::row(s, &format!("thread-{i}"), i == 0).child(label(s, t, 13., i != 0));
        side = side.child(r);
    }
    let composer = kit::floating(&st, p).apply(
        row().key("composer").w(720.).h(96.).radius(super::BUBBLE_RADIUS).padding(14.)
            .child(label(s, "Ask anything…", 14., true)),
    );
    let panel = col()
        .grow(1.)
        .h_full()
        .padding(8.)
        .child(
            col().w_full().h_full().radius(super::PANEL_RADIUS).fill(p.bg).center().gap(16.)
                .child(kit::claw_mark(s))
                .child(label(s, "What are we building?", 22., false))
                .child(composer),
        );
    let mut window = row().w_full().h_full().fill(p.surface).child(side).child(panel);
    if let Some(layer) = super::look::wallpaper_layer(s, "window") {
        window = window.child(layer);
    }
    window
}

fn section(s: &State, title: &str, body: El) -> El {
    col().gap(8.).child(label(s, title, 12., true)).child(body)
}

fn gallery(s: &State) -> El {
    let p = &s.palette;
    let st = Style::of(&s.prefs);
    let noop = |_: &mut State| {};

    let mut buttons = col().gap(10.);
    for (name, size) in [
        ("xs", kit::Size::XSmall),
        ("sm", kit::Size::Small),
        ("md", kit::Size::Medium),
        ("lg", kit::Size::Large),
    ] {
        let mut line = row().gap(8.).center_y();
        for (v, variant) in [
            ("primary", kit::Variant::Primary),
            ("secondary", kit::Variant::Secondary),
            ("outline", kit::Variant::Outline),
            ("ghost", kit::Variant::Ghost),
            ("danger", kit::Variant::Danger),
        ] {
            line = line.child(
                Button::new(format!("b-{name}-{v}")).label(v).icon(&icons::PLUS).variant(variant)
                    .size(size).on_press(noop).build(s),
            );
        }
        line = line.child(
            Button::new(format!("b-{name}-icon")).icon(&icons::SEARCH).ghost().size(size)
                .tooltip("Search").build(s),
        );
        buttons = buttons.child(line);
    }
    buttons = buttons.child(
        row().gap(8.)
            .child(Button::new("b-sel").label("Selected").selected(true).build(s))
            .child(Button::new("b-sel-o").label("Selected").outline().selected(true).build(s))
            .child(Button::new("b-dis").label("Disabled").primary().disabled(true).build(s))
            .child(Button::new("b-load").label("Saving").loading(true).build(s)),
    );

    let mut rows = col().w(280.).gap(2.);
    if let Some(pill) = kit::slide_pill(s, "pill", Some(st.row_height + 2.)) {
        rows = rows.child(pill);
    }
    for (i, t) in ["Inbox", "Automations", "Skills"].iter().enumerate() {
        let (ink, hover) = kit::row_ink(s, i == 1);
        rows = rows.child(
            kit::pill_row(s, &format!("prow-{i}"), i == 1)
                .child(rune::icon(&icons::INBOX, 16.).color(ink).group_color(hover))
                .child(text(t.to_string()).font(TextStyle::new(13., 400.)).color(ink).group_color(hover))
                .child(el().grow(1.))
                .child(kit::shortcut_badge(s, i)),
        );
    }
    let draft = kit::row(s, "row-draft", false).fill(kit::draft_wash(p))
        .child(label(s, "A row with a draft", 13., false));
    rows = rows.child(draft);

    let controls = col().gap(10.)
        .child(kit::segmented(s, "seg", &["System", "Light", "Dark"], 2, false, |_s, _i| {}))
        .child(kit::segmented(s, "seg-off", &["On", "Off"], 0, true, |_s, _i| {}))
        .child(
            row().gap(6.)
                .child(kit::chip(s, "chip-a", "Running", true))
                .child(kit::chip(s, "chip-b", "Done", false))
                .child(kit::chip(s, "chip-c", "Failed", false)),
        );

    let loops = row().gap(18.).center_y()
        .child(kit::pulse_dots("dots", p.text_muted))
        .child(kit::gradient_spinner("gspin", 4.))
        .child(kit::status_dot("dot-live", p.success, true))
        .child(kit::status_dot("dot", p.warning, false))
        .child(kit::shimmer_text(s, "shimmer", "Thinking…", TextStyle::new(13., 400.)));

    let mut logos = row().gap(10.).center_y();
    for id in LOGOS {
        logos = logos.child(brands::provider_logo(s, &[id], 20.));
    }

    let chrome = row().gap(8.).center_y()
        .child(panels::toggle(s, "tg-left", true, false, "Hide sidebar"))
        .child(panels::toggle(s, "tg-right", false, true, "Show right pane"))
        .child(panels::glyph("g-open", true, false, 24., p.text))
        .child(panels::glyph("g-shut", false, false, 24., p.text))
        .child(panels::card_glyph("g-card", true, 24., p.text))
        .child(kit::claw_mark(s));

    let card = kit::card(s).w(360.)
        .child(label(s, "Approve this command?", 14., false))
        .child(kit::activity_line(
            s,
            "act",
            &icons::FILE_CODE,
            "Ran cargo build",
            Some("Compiling ironclaw_rune v0.1.0 (tools/ironclaw_rune/app)\nFinished dev profile in 5.54 s"),
        ))
        .child(
            row().gap(8.)
                .child(Button::new("c-yes").label("Approve").primary().small().build(s))
                .child(Button::new("c-no").label("Reject").small().build(s)),
        );
    let pop = kit::popover(s, "pop", col().w(220.).padding(6.).gap(2.)
        .child(kit::row(s, "pop-a", true).child(label(s, "Rename", 13., false)))
        .child(kit::row(s, "pop-b", false).child(label(s, "Delete", 13., false))));
    let glass = kit::glass(&st, p).apply(col().w(220.).padding(12.).radius(12.)
        .child(label(s, "Glass card", 13., false)));

    let left = col().gap(22.)
        .child(section(s, "BUTTONS", buttons))
        .child(section(s, "CONTROLS", controls))
        .child(section(s, "LOOPS", loops))
        .child(section(s, "PROVIDERS", logos));
    let right = col().gap(22.)
        .child(section(s, "ROWS", rows))
        .child(section(s, "PANEL CHROME", chrome))
        .child(section(s, "CARD", card))
        .child(section(s, "POPOVER · GLASS", row().gap(12.).items_start().child(pop).child(glass)));
    row().w_full().h_full().fill(p.bg).padding(28.).gap(36.).child(left).child(right)
}
