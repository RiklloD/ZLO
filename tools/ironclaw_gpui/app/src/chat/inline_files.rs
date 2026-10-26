//! Files placed in the text. A staged file shows as a card at the spot it
//! was attached: in the composer it is an atomic inline token (gpui-kit's
//! inline tokens: one Backspace removes it, undo brings it back), and in the
//! sent bubble a card between the words. On send each token becomes an
//! anchor (`{{attachment:N}}`), so the backend puts the file's reference
//! where the person put it. Both composers (thread and new-thread draft)
//! share these helpers.

use std::collections::HashSet;
use std::path::PathBuf;

use ironclaw_gpui_client::attachments::{self, StagedFile};
use ironclaw_gpui_client::inbound_requests::attachment_anchor;

use crate::design::Palette;
use crate::snapshot;
use crate::ui::*;

/// Put a card for each of `new` (already pushed onto `all`) at the
/// composer's cursor, each followed by a space. A SnapShot's text half
/// rides with its picture and gets none.
pub(crate) fn insert(
    composer: &Entity<TextareaState>,
    new: &[StagedFile],
    all: &[StagedFile],
    window: &mut Window,
    cx: &mut App,
) {
    let tokens: Vec<InlineToken> = new
        .iter()
        .filter(|f| !snapshot::is_companion(f, all))
        .map(|f| {
            let (title, label) =
                snapshot::names(f, cx).unwrap_or_else(|| (f.filename.clone(), f.filename.clone()));
            InlineToken::new(f.key.to_string(), printable(&title)).with_label(printable(&label))
        })
        .collect();
    composer.update(cx, |state, cx| {
        for token in tokens {
            match state.replace_with_token(token, window, cx) {
                Ok(()) => state.insert(" ", window, cx),
                Err(error) => tracing::debug!(%error, "composer: file card not placed"),
            }
        }
        state.focus(window, cx);
    });
}

/// Drop the staged files whose card is gone from the composer (a SnapShot's
/// text half goes with its picture).
pub(crate) fn prune(composer: &Entity<TextareaState>, staged: &mut Vec<StagedFile>, cx: &App) {
    let live: HashSet<String> = composer
        .read(cx)
        .tokens()
        .iter()
        .map(|span| span.token().id().to_string())
        .collect();
    let kept: Vec<bool> = staged
        .iter()
        .map(|f| live.contains(&f.key.to_string()))
        .collect();
    let mut ix = 0;
    let all = staged.clone();
    staged.retain(|f| {
        let keep = kept[ix]
            || all
                .iter()
                .zip(&kept)
                .any(|(image, kept)| *kept && snapshot::pairs(f, image));
        ix += 1;
        keep
    });
}

/// What to send: the text with an anchor at each card, and the files in
/// anchor order (a SnapShot's text half right after its picture, with no
/// anchor of its own). A card whose file is gone sends its name as text.
pub(crate) fn outgoing(
    composer: &Entity<TextareaState>,
    staged: &[StagedFile],
    cx: &App,
) -> (String, Vec<StagedFile>) {
    let content = composer.read(cx).content();
    let text = content.text();
    let mut files: Vec<StagedFile> = Vec::new();
    let mut out = String::with_capacity(text.len());
    let mut last = 0;
    for span in content.tokens() {
        let range = span.range();
        out.push_str(&text[last..range.start]);
        last = range.end;
        let id = span.token().id();
        let Some(file) = staged.iter().find(|f| f.key.to_string() == id.as_ref()) else {
            out.push_str(span.token().text());
            continue;
        };
        let slot = match files.iter().position(|f| f.key == file.key) {
            Some(slot) => slot,
            None => {
                files.push(file.clone());
                let slot = files.len() - 1;
                files.extend(
                    staged
                        .iter()
                        .filter(|text| snapshot::pairs(text, file))
                        .cloned(),
                );
                slot
            }
        };
        out.push_str(&attachment_anchor(slot));
    }
    out.push_str(&text[last..]);
    (out.trim().to_string(), files)
}

/// What a paste brings that becomes a card: files copied in Finder, or a
/// picture (a screenshot copied to the clipboard).
pub(crate) enum Pasted {
    Paths(Vec<PathBuf>),
    Image { bytes: Vec<u8>, mime: &'static str },
}

/// `None` for a text paste, which the composer inserts as usual.
pub(crate) fn pasted(item: &ClipboardItem) -> Option<Pasted> {
    item.entries().iter().find_map(|entry| match entry {
        ClipboardEntry::ExternalPaths(paths) if !paths.paths().is_empty() => {
            Some(Pasted::Paths(paths.paths().to_vec()))
        }
        ClipboardEntry::Image(image) => Some(Pasted::Image {
            bytes: image.bytes.clone(),
            mime: image.format.mime_type(),
        }),
        _ => None,
    })
}

/// A pasted picture as a file to upload, within the gateway's limits.
pub(crate) fn pasted_image(
    bytes: Vec<u8>,
    mime: &str,
    staged: &[StagedFile],
    limits: &ironclaw_gpui_client::wire::AttachmentLimits,
) -> Result<StagedFile, String> {
    let ext = mime.rsplit('/').next().unwrap_or("png");
    let filename = format!("Pasted image.{ext}");
    let mime = attachments::mime_for(&filename, limits)
        .ok_or("This gateway does not accept pasted pictures of this type.")?;
    let file = StagedFile::from_bytes(filename, mime, bytes);
    attachments::check(&file, staged, limits)?;
    Ok(file)
}

/// The composer's renderer for its file tokens.
pub(crate) fn token_renderer(
    staged: &[StagedFile],
    cx: &App,
) -> impl Fn(&InlineTokenContext, &mut Window, &mut App) -> AnyElement + 'static {
    let snapshots: Vec<(String, StagedFile)> = staged
        .iter()
        .filter(|f| snapshot::icon(f, cx).is_some())
        .map(|f| (f.key.to_string(), f.clone()))
        .collect();
    move |token, _, cx| {
        let file = snapshots
            .iter()
            .find(|(key, _)| key == token.token().id().as_ref())
            .map(|(_, f)| f);
        let icon = file.and_then(|f| snapshot::icon(f, cx));
        inline_card(
            SharedString::from(format!("token-{}", token.token().id())),
            icon,
            token.token().text(),
            token.line_height(),
            token.is_selected(),
            cx,
        )
        .into_any_element()
    }
}

/// Token text may hold no control characters.
fn printable(text: &str) -> String {
    text.chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect()
}

/// The one-line card that sits inside text: icon tile and name, the same
/// look as the full attachment card, at the height of a line.
pub(crate) fn inline_card(
    id: SharedString,
    icon: Option<AnyElement>,
    title: &str,
    line_height: Pixels,
    selected: bool,
    cx: &App,
) -> gpui_kit::Stateful<gpui_kit::Div> {
    let p = Palette::of(cx);
    let tile = (line_height - px(6.)).max(px(12.));
    h_flex()
        .id(id)
        .h(line_height - px(2.))
        .my(px(1.))
        .gap(px(5.))
        .pl(px(2.))
        .pr(px(7.))
        .items_center()
        .rounded(px(7.))
        .border_1()
        .border_color(if selected {
            cx.theme().ring
        } else {
            p.border_strong
        })
        .bg(p.hover)
        .child(
            div()
                .size(tile)
                .flex_none()
                .flex()
                .items_center()
                .justify_center()
                .rounded(px(4.))
                .border_1()
                .border_color(p.border_strong)
                .child(
                    icon.unwrap_or_else(|| Icon::new(IconName::File).xsmall().into_any_element()),
                ),
        )
        .child(
            div()
                .max_w(px(240.))
                .text_sm()
                .font_medium()
                .truncate()
                .child(title.to_string()),
        )
}
